//! Meta-key/codec roundtrips, registry semantics, placement and the
//! writer's file + rebuild behavior (M2 scope).

use super::meta::{
    self, decode_meta, encode_meta, meta_key, parse_meta_key, SegmentColumnZone, SegmentMeta,
    SegmentState,
};
use super::writer;
use super::{rebuild, registry_of, Registry};
use crate::sql::storage::codec::KIND_SQL_SEGMENT;
use crate::sql::storage::schema::{ColumnDef, Engine, SqlType, TableSchema, Value};
use crate::state::testutil;

fn zone(name: &str, null_count: u64, min: Value, max: Value) -> SegmentColumnZone {
    SegmentColumnZone {
        name: name.into(),
        null_count,
        min,
        max,
    }
}

fn fixture_meta(table_id: u32, segment_id: u64, state: SegmentState) -> SegmentMeta {
    SegmentMeta {
        table_id,
        table_name: format!("t{table_id}"),
        segment_id,
        commit_ts: 100 + segment_id,
        state,
        num_rows: 2,
        file: writer::segment_file_name(table_id, segment_id),
        columns: vec![
            zone("id", 0, Value::Int(1), Value::Int(9)),
            zone("x", 1, Value::Null, Value::Bytes(vec![0xFF, 0x01])),
        ],
    }
}

#[test]
fn meta_key_roundtrip() {
    for (table_id, segment_id) in [(0u32, 0u64), (1, 1), (7, 42), (u32::MAX, u64::MAX)] {
        let key = meta_key(table_id, segment_id);
        assert_eq!(key.len(), 13);
        assert_eq!(key[0], KIND_SQL_SEGMENT);
        assert_eq!(parse_meta_key(&key), Some((table_id, segment_id)));
    }
}

#[test]
fn parse_meta_key_rejects_wrong_kind_and_length() {
    let mut key = meta_key(7, 42);
    key[0] = KIND_SQL_SEGMENT - 1;
    assert_eq!(parse_meta_key(&key), None, "wrong kind byte");
    let good = meta_key(7, 42);
    assert_eq!(parse_meta_key(&good[..12]), None, "too short");
    let mut long = good.clone();
    long.push(0);
    assert_eq!(parse_meta_key(&long), None, "too long");
    assert_eq!(parse_meta_key(&[]), None, "empty");
}

#[test]
fn meta_encode_decode_roundtrip_incl_null_and_bytes_zones() {
    for state in [SegmentState::Prepared, SegmentState::Live] {
        let meta = fixture_meta(3, 9, state);
        let enc = encode_meta(&meta).unwrap();
        assert!(enc.len() <= meta::MAX_META_BYTES);
        assert_eq!(decode_meta(&enc).unwrap(), meta);
    }
}

#[test]
fn decode_meta_rejects_garbage_and_oversized() {
    assert!(decode_meta(b"not json").is_err());
    let big = vec![b'0'; meta::MAX_META_BYTES + 1];
    assert!(decode_meta(&big).is_err());
    let meta = fixture_meta(1, 1, SegmentState::Live);
    assert!(encode_meta(&meta).unwrap().len() <= meta::MAX_META_BYTES);
}

#[test]
fn registry_insert_upserts_and_keeps_sorted() {
    let reg = Registry::default();
    assert_eq!(reg.max_table_id(), 0);
    reg.insert(&fixture_meta(5, 30, SegmentState::Live));
    reg.insert(&fixture_meta(2, 9, SegmentState::Prepared));
    reg.insert(&fixture_meta(5, 10, SegmentState::Live));
    let ids: Vec<u64> = reg.segments(5).iter().map(|m| m.segment_id).collect();
    assert_eq!(ids, vec![10, 30], "sorted by segment_id");
    // upsert: same (table, segment) replaces in place (state flip)
    let flipped = fixture_meta(5, 30, SegmentState::Live);
    reg.insert(&SegmentMeta {
        state: SegmentState::Prepared,
        ..flipped.clone()
    });
    assert_eq!(reg.segments(5)[1].state, SegmentState::Prepared);
    reg.insert(&flipped);
    assert_eq!(reg.segments(5)[1].state, SegmentState::Live);
    assert_eq!(reg.segments(5).len(), 2, "upsert, not duplicate");
    // all() walks tables ascending, segments sorted within
    let all = reg.all();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].table_id, 2);
    assert_eq!(reg.max_table_id(), 5);
    reg.remove(5, &[30]);
    assert_eq!(reg.segments(5).len(), 1);
    reg.remove(5, &[10]);
    assert!(reg.segments(5).is_empty(), "empty table list dropped");
    assert_eq!(reg.max_table_id(), 2);
    reg.drop_table(2);
    assert_eq!(reg.all().len(), 0);
    assert_eq!(reg.max_table_id(), 0);
}

#[test]
fn choose_segment_id_finds_band_and_errors_when_none() {
    let table_id = 5u32;
    let id = writer::choose_segment_id(table_id, 1000, &|slot| slot <= 8191).unwrap();
    assert!(id >= 1000);
    assert!(
        meta::segment_slot(table_id, id) <= 8191,
        "picked id must hash into the allowed band"
    );
    let err = writer::choose_segment_id(table_id, 0, &|_| false).unwrap_err();
    assert!(err.contains("no eligible slot band"), "{err}");
}

fn two_col_schema() -> TableSchema {
    TableSchema {
        id: 21,
        name: "wm".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                sql_type: SqlType::Int,
                nullable: false,
            },
            ColumnDef {
                name: "s".into(),
                sql_type: SqlType::VarChar,
                nullable: true,
            },
        ],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Columnar,
        indexes: vec![],
    }
}

#[test]
fn write_segment_file_leaves_no_tmp_and_decodes_back() {
    let dir = tempfile::tempdir().unwrap();
    let schema = two_col_schema();
    let rows = vec![
        vec![Value::Int(1), Value::Str("a".into())],
        vec![Value::Int(2), Value::Null],
        vec![Value::Int(3), Value::Str("b".into())],
    ];
    let (name, zones, n) = writer::write_segment_file(dir.path(), &schema, 99, &rows).unwrap();
    assert_eq!(n, 3);
    assert_eq!(name, writer::segment_file_name(21, 99));
    assert_eq!(zones.len(), 2);
    for e in dir.path().read_dir().unwrap() {
        let f = e.unwrap().file_name().into_string().unwrap();
        assert!(!f.ends_with(".tmp"), "tmp file left behind: {f}");
    }
    let file = std::fs::read(dir.path().join(&name)).unwrap();
    let (footer, _) = crate::sql::columnar::decode::open(&file).unwrap();
    assert_eq!(footer.columns.len(), 2);
    let c0 = crate::sql::columnar::decode::decode_column(&file, &footer.columns[0], SqlType::Int)
        .unwrap();
    let c1 =
        crate::sql::columnar::decode::decode_column(&file, &footer.columns[1], SqlType::VarChar)
            .unwrap();
    let back: Vec<Vec<Value>> = (0..rows.len())
        .map(|i| vec![c0[i].clone(), c1[i].clone()])
        .collect();
    assert_eq!(back, rows);
}

#[test]
fn flush_limit_accessors_default_and_explicit() {
    let mut conf = crate::conf::Config::default();
    assert_eq!(writer::flush_rows_limit(&conf), writer::DEFAULT_FLUSH_ROWS);
    assert_eq!(
        writer::flush_bytes_limit(&conf),
        writer::DEFAULT_FLUSH_BYTES
    );
    conf.columnar_flush_rows = 7;
    conf.columnar_flush_bytes = 99;
    assert_eq!(writer::flush_rows_limit(&conf), 7);
    assert_eq!(writer::flush_bytes_limit(&conf), 99);
}

#[test]
fn rebuild_and_registry_of_recover_metas_from_store() {
    let shared = testutil::shared_with(testutil::test_config());
    let live = fixture_meta(3, 77, SegmentState::Live);
    let prepared = fixture_meta(3, 78, SegmentState::Prepared);
    let mut batch = rocksdb::WriteBatch::default();
    for m in [&live, &prepared] {
        batch.put(meta_key(m.table_id, m.segment_id), encode_meta(m).unwrap());
    }
    crate::store::ops::batch_write(&shared.store, batch).unwrap();
    let fresh = rebuild(&shared.store);
    assert_eq!(fresh.segments(3), vec![live.clone(), prepared.clone()]);
    // registry_of caches per instance, but sees the same store state
    let cached = registry_of(&shared);
    assert_eq!(cached.segments(3).len(), 2);
    assert_eq!(cached.max_table_id(), 3);
}

#[tokio::test]
async fn drop_table_segments_purges_metas_registry_and_files() {
    let shared = testutil::shared_with(testutil::test_config());
    let schema = two_col_schema(); // table id 21
    let rows = vec![
        vec![Value::Int(1), Value::Str("a".into())],
        vec![Value::Int(2), Value::Null],
    ];
    let m1 = writer::commit_segment(&shared, &schema, 11, 10, &rows)
        .await
        .unwrap();
    let m2 = writer::commit_segment(&shared, &schema, 12, 20, &rows)
        .await
        .unwrap();
    // A neighbor table's meta must survive the prefix-bounded walk.
    let mut other = two_col_schema();
    other.id = 22;
    other.name = "neighbor".into();
    let m3 = writer::commit_segment(&shared, &other, 13, 30, &rows)
        .await
        .unwrap();
    let dir = writer::columnar_dir(&shared.conf);
    assert!(dir.join(&m1.file).exists() && dir.join(&m2.file).exists());

    super::commit::drop_table_segments(&shared, schema.id)
        .await
        .unwrap();

    for m in [&m1, &m2] {
        assert!(
            crate::store::ops::get_physical(&shared.store, &meta_key(m.table_id, m.segment_id))
                .unwrap()
                .is_none(),
            "meta of segment {} must be deleted",
            m.segment_id
        );
        assert!(
            !dir.join(&m.file).exists(),
            "file {} must be deleted",
            m.file
        );
    }
    assert!(registry_of(&shared).segments(schema.id).is_empty());
    // the neighbor table is untouched
    assert_eq!(registry_of(&shared).segments(other.id), vec![m3.clone()]);
    assert!(
        crate::store::ops::get_physical(&shared.store, &meta_key(m3.table_id, m3.segment_id))
            .unwrap()
            .is_some()
    );
    assert!(dir.join(&m3.file).exists());

    // a second call is a no-op
    super::commit::drop_table_segments(&shared, schema.id)
        .await
        .unwrap();
    assert!(registry_of(&shared).segments(schema.id).is_empty());
}
