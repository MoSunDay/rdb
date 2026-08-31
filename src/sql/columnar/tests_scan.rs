//! M3 read path tests: segment decoding and local scan visibility.

use super::meta::SegmentState;
use super::{encode, reader, registry_of, writer};
use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel, SqlType, TableSchema, Value};
use crate::state::testutil;

fn col(name: &str, sql_type: SqlType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        sql_type,
        nullable,
    }
}

fn columnar_schema(id: u32) -> TableSchema {
    TableSchema {
        id,
        name: format!("ct{id}"),
        columns: vec![
            col("id", SqlType::Int, false),
            col("tag", SqlType::VarChar, true),
        ],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Columnar,
        indexes: vec![],
        key_model: KeyModel::MySql,
        distribution: None,
    }
}

fn build_file(schema: &TableSchema, rows: &[Vec<Value>]) -> Vec<u8> {
    // `build_segment` encodes every column page and assembles the
    // final file (format::assemble_file) in one step.
    let (file, _) = encode::build_segment(schema, rows).unwrap();
    file
}

fn row(i: i64, tag: &str) -> Vec<Value> {
    vec![Value::Int(i), Value::Str(tag.into())]
}

#[test]
fn decode_segment_roundtrips_nulls_and_dict_strings() {
    let schema = columnar_schema(1);
    // Low-cardinality repeated strings (dict-eligible) with NULLs.
    let rows: Vec<Vec<Value>> = (0..32)
        .map(|i| {
            let tag = if i % 5 == 0 {
                Value::Null
            } else {
                Value::Str(format!("tag{}", i % 3))
            };
            vec![Value::Int(i), tag]
        })
        .collect();
    let file = build_file(&schema, &rows);
    assert_eq!(reader::decode_segment(&file, &schema).unwrap(), rows);
}

#[test]
fn decode_segment_rejects_width_mismatch() {
    let two = columnar_schema(2);
    let file = build_file(&two, &[vec![Value::Int(1), Value::Null]]);
    // Same first two columns plus an extra third one.
    let three = TableSchema {
        id: 3,
        name: "three".into(),
        columns: vec![
            col("id", SqlType::Int, false),
            col("tag", SqlType::VarChar, true),
            col("extra", SqlType::Double, true),
        ],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Columnar,
        indexes: vec![],
        key_model: KeyModel::MySql,
        distribution: None,
    };
    let err = reader::decode_segment(&file, &three).unwrap_err();
    assert!(err.msg.contains("width mismatch"), "{}", err.msg);
}

#[tokio::test]
async fn scan_local_orders_segments_and_applies_read_ts() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = columnar_schema(4);
    // Commit ts 20 FIRST, then two ts-10 segments: the scan order is
    // (commit_ts, segment_id), not the commit order.
    writer::commit_segment(&shared, &s, 7, 20, &[row(2, "later")])
        .await
        .unwrap();
    writer::commit_segment(&shared, &s, 9, 10, &[row(1, "earlier")])
        .await
        .unwrap();
    writer::commit_segment(&shared, &s, 3, 10, &[row(0, "first")])
        .await
        .unwrap();
    let got = reader::scan_local(&shared, &s, 100, None).unwrap();
    assert_eq!(
        got,
        vec![row(0, "first"), row(1, "earlier"), row(2, "later")]
    );
    // read_ts cutoff hides the ts-20 segment.
    let got = reader::scan_local(&shared, &s, 15, None).unwrap();
    assert_eq!(got, vec![row(0, "first"), row(1, "earlier")]);
    // An open txn's overlay rides after every segment row.
    let got = reader::scan_local(&shared, &s, 100, Some(&[row(5, "own")])).unwrap();
    assert_eq!(got.len(), 4);
    assert_eq!(got.last(), Some(&row(5, "own")));
}

#[tokio::test]
async fn scan_local_empty_table_is_empty() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = columnar_schema(5);
    assert!(registry_of(&shared).segments(s.id).is_empty());
    assert!(reader::scan_local(&shared, &s, 10, None)
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn scan_local_skips_prepared_metas() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = columnar_schema(6);
    let rows = [row(1, "prepared")];
    let dir = writer::columnar_dir(&shared.conf);
    let (file, columns, num_rows) = writer::write_segment_file(&dir, &s, 42, &rows).unwrap();
    let meta = writer::build_meta(&s, 42, 5, SegmentState::Prepared, file, columns, num_rows);
    registry_of(&shared).insert(&meta);
    assert!(
        reader::scan_local(&shared, &s, 100, None)
            .unwrap()
            .is_empty(),
        "Prepared segments are never visible"
    );
}

#[tokio::test]
async fn table_rows_resolves_schema_by_id() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = columnar_schema(8);
    shared.raft.write().unwrap().kv.insert(
        crate::sql::storage::catalog::catalog_key(&s.name),
        serde_json::to_string(&s).unwrap(),
    );
    writer::commit_segment(&shared, &s, 1, 9, &[row(1, "a")])
        .await
        .unwrap();
    assert_eq!(
        reader::table_rows(&shared, 8, 9).unwrap(),
        vec![row(1, "a")]
    );
    assert!(reader::table_rows(&shared, 99, 9).is_err(), "unknown id");
    // A row-engine table under that id is rejected.
    let mut row_table = columnar_schema(9);
    row_table.engine = Engine::Row;
    shared.raft.write().unwrap().kv.insert(
        crate::sql::storage::catalog::catalog_key(&row_table.name),
        serde_json::to_string(&row_table).unwrap(),
    );
    assert!(reader::table_rows(&shared, 9, 9).is_err(), "not columnar");
}

/// Regression (cluster INSERT data loss): three consecutive autocommit
/// columnar INSERTs served by ONE node of a ready cluster. Segment ids
/// are scanned forward from each statement's commit ts to the next
/// slot band this node owns -- a mapping that is NOT injective: when
/// the base's own slot is foreign, the next statement (base+1) scans
/// to the very same id, and its meta key / segment file / registry
/// upsert silently replace the first batch's segment. The real three
/// node cluster showed COUNT(*) 3 -> 4 -> 7 with batch 1's rows gone;
/// every batch must keep its own segment (3 -> 7 -> 10 here).
#[tokio::test]
async fn three_autocommit_inserts_keep_every_batch_visible() {
    use crate::sql::columnar::meta;
    use crate::sql::exec::{ddl, write, SqlSession};
    use crate::sql::parse::parse_statement;
    use crate::topology;

    let shared = testutil::shared_with(testutil::test_config());
    let foreign = "127.0.0.1:32712".to_string();
    let third = "127.0.0.1:32713".to_string();
    // Ready three member cluster; THIS node is addrs[0] (band owner of
    // the low slots), exactly like the raft leader in the repro.
    *shared.topology.write().unwrap() = topology::refresh(&format!(
        "{},{},{}",
        shared.conf.bind, foreign, third
    ));
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE cl (k BIGINT PRIMARY KEY) ENGINE=columnar").unwrap(),
    )
    .await
    .unwrap();
    let schema = crate::sql::storage::catalog::lookup(&shared, "cl")
        .unwrap()
        .expect("table exists");
    assert!(schema.engine.is_columnar());
    // The next statement's commit ts is the id scan's base: make its
    // slot FOREIGN (the old chooser jumped forward) and the following
    // candidates' slots LOCAL (the jump was short, base+1 in the real
    // cluster), so batch 2's base scans onto batch 1's chosen id.
    let base = shared.sql_ts.now() + 1;
    {
        let mut topo = shared.topology.write().unwrap();
        for c in base..base + 32 {
            let slot = meta::segment_slot(schema.id, c);
            let owner = if c == base {
                foreign.clone()
            } else {
                shared.conf.bind.clone()
            };
            topo.owner_map.insert(slot, owner);
        }
    }
    let mut sess = SqlSession::default();
    let batches = [
        ("INSERT INTO cl (k) VALUES (1), (2), (3)", 3_i64),
        ("INSERT INTO cl (k) VALUES (4), (5), (6), (7)", 7),
        ("INSERT INTO cl (k) VALUES (8), (9), (10)", 10),
    ];
    for (sql, expected) in batches {
        write::insert(&shared, &mut sess, parse_statement(sql).unwrap())
            .await
            .unwrap();
        let rows = reader::table_rows(&shared, schema.id, shared.sql_ts.now()).unwrap();
        let mut ks: Vec<i64> = rows
            .into_iter()
            .map(|r| match &r[0] {
                Value::Int(k) => *k,
                other => panic!("unexpected key {other:?}"),
            })
            .collect();
        ks.sort();
        assert_eq!(
            ks.len(),
            expected as usize,
            "COUNT(*) after '{sql}': earlier batches must stay visible"
        );
        assert_eq!(
            *ks.last().unwrap(), expected,
            "batch '{sql}' appended its own rows"
        );
    }
    // One segment per batch, all distinct ids, all rows present.
    let segs = registry_of(&shared).segments(schema.id);
    let mut ids: Vec<u64> = segs.iter().map(|m| m.segment_id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "three batches, three distinct segments");
}
