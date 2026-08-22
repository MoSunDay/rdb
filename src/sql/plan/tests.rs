//! Planner unit tests: index choice, sargability, fallbacks.

use super::*;
use crate::sql::parse::parse_statement;
use crate::sql::storage::schema::{ColumnDef, IndexDef, SqlType};
use crate::state::testutil;
use crate::state::Shared;

fn schema() -> TableSchema {
    TableSchema {
        id: 7,
        name: "t".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                sql_type: SqlType::Int,
                nullable: false,
            },
            ColumnDef {
                name: "v".into(),
                sql_type: SqlType::VarChar,
                nullable: true,
            },
            ColumnDef {
                name: "n".into(),
                sql_type: SqlType::Int,
                nullable: true,
            },
        ],
        pk: "id".into(),
        indexes: vec![IndexDef {
            id: 1,
            name: "idx_v".into(),
            column: "v".into(),
            unique: false,
        }],
    }
}

fn filter_of(sql: &str) -> Expr {
    let stmt = parse_statement(sql).expect("parse");
    let crate::sql::parse::ast::Statement::Select(q) = stmt else {
        panic!("select");
    };
    q.filter.expect("filter")
}

fn shared() -> Shared {
    testutil::shared_with(testutil::test_config())
}

fn seed(shared: &Shared, rows: &[(i64, &str)]) {
    let s = schema();
    let mut batch = rocksdb::WriteBatch::default();
    for (i, (id, v)) in rows.iter().enumerate() {
        let values = vec![Value::Int(*id), Value::Str(v.to_string()), Value::Int(*id)];
        let pk = crate::sql::storage::row::pk_encode(&Value::Int(*id)).unwrap();
        batch.put(
            crate::sql::storage::row::version_key(
                &s,
                crate::sql::storage::row::row_slot(&s, &pk),
                &pk,
                1 + i as u64,
            ),
            crate::sql::storage::row::encode_row(&s, &values).unwrap(),
        );
        // index entries for idx_v
        let index = IndexRef::of(&s.indexes[0]);
        for (k, val) in index::entries_for_live_row(
            &s,
            &index,
            &pk,
            &[Value::Int(*id), Value::Str(v.to_string()), Value::Int(*id)],
        )
        .unwrap()
        {
            batch.put(k, val.unwrap_or_default());
        }
    }
    crate::store::ops::batch_write(&shared.store, batch).unwrap();
}

fn pk(id: i64) -> Vec<u8> {
    crate::sql::storage::row::pk_encode(&Value::Int(id)).unwrap()
}

#[test]
fn eq_pick_index_and_seqscan_fallbacks() {
    let sh = shared();
    seed(&sh, &[(1, "red"), (2, "red"), (3, "blue")]);
    let s = schema();
    // eq on the indexed column -> index
    let path = plan(
        &sh.store,
        &s,
        None,
        Some(&filter_of("SELECT 1 FROM t WHERE v = 'red'")),
    );
    match path {
        Path::IndexLookup { index, pks } => {
            assert_eq!(index.name, "idx_v");
            assert_eq!(pks, vec![pk(1), pk(2)]);
        }
        other => panic!("expected index, got {other:?}"),
    }
    // unindexed column / no filter / OR / unindexed predicate
    assert!(matches!(
        plan(
            &sh.store,
            &s,
            None,
            Some(&filter_of("SELECT 1 FROM t WHERE n = 1"))
        ),
        Path::SeqScan
    ));
    assert!(matches!(plan(&sh.store, &s, None, None), Path::SeqScan));
    assert!(matches!(
        plan(
            &sh.store,
            &s,
            None,
            Some(&filter_of("SELECT 1 FROM t WHERE v = 'red' OR v = 'blue'"))
        ),
        Path::SeqScan
    ));
    // qualified by table name works, by alias works, wrong table no
    let p = plan(
        &sh.store,
        &s,
        None,
        Some(&filter_of("SELECT 1 FROM t WHERE t.v = 'blue'")),
    );
    assert!(matches!(p, Path::IndexLookup { .. }));
    let p = plan(
        &sh.store,
        &s,
        Some("x"),
        Some(&filter_of("SELECT 1 FROM t WHERE x.v = 'blue'")),
    );
    assert!(matches!(p, Path::IndexLookup { .. }));
    let p = plan(
        &sh.store,
        &s,
        Some("x"),
        Some(&filter_of("SELECT 1 FROM t WHERE o.v = 'blue'")),
    );
    assert!(matches!(p, Path::SeqScan));
}

#[test]
fn in_between_and_wide_fallback() {
    let sh = shared();
    let s = schema();
    let mut rows = Vec::new();
    for i in 0..50 {
        rows.push((i, if i % 2 == 0 { "even" } else { "odd" }));
    }
    seed(&sh, &rows);
    let p = plan(
        &sh.store,
        &s,
        None,
        Some(&filter_of("SELECT 1 FROM t WHERE v IN ('even', 'zzz')")),
    );
    match p {
        Path::IndexLookup { pks, .. } => assert_eq!(pks.len(), 25),
        other => panic!("{other:?}"),
    }
    let p = plan(
        &sh.store,
        &s,
        None,
        Some(&filter_of(
            "SELECT 1 FROM t WHERE v BETWEEN 'even' AND 'odd'",
        )),
    );
    match p {
        Path::IndexLookup { pks, .. } => assert_eq!(pks.len(), 50),
        other => panic!("{other:?}"),
    }
    // BETWEEN reversed -> empty range ok; planner still picks index
    let p = plan(
        &sh.store,
        &s,
        None,
        Some(&filter_of(
            "SELECT 1 FROM t WHERE v BETWEEN 'zzz' AND 'aaa'",
        )),
    );
    assert!(matches!(p, Path::IndexLookup { .. }));
    // no index at all
    let mut no_idx = schema();
    no_idx.indexes.clear();
    assert!(matches!(
        plan(
            &sh.store,
            &no_idx,
            None,
            Some(&filter_of("SELECT 1 FROM t WHERE v = 'even'"))
        ),
        Path::SeqScan
    ));
    // wide result -> seqscan
    let mut wide = schema();
    wide.indexes[0].column = "n".into();
    let mut many = Vec::new();
    for i in 0..1200 {
        many.push((i, "x"));
    }
    let sh2 = shared();
    // entries for the n-column index (col pos 2)
    let mut batch = rocksdb::WriteBatch::default();
    for (id, _) in &many {
        let values = vec![Value::Int(*id), Value::Str("x".into()), Value::Int(*id)];
        let pk_key = pk(*id);
        batch.put(
            crate::sql::storage::row::version_key(
                &wide,
                crate::sql::storage::row::row_slot(&wide, &pk_key),
                &pk_key,
                1,
            ),
            crate::sql::storage::row::encode_row(&wide, &values).unwrap(),
        );
        let index = IndexRef::of(&wide.indexes[0]);
        for (k, val) in index::entries_for_live_row(&wide, &index, &pk_key, &values).unwrap() {
            batch.put(k, val.unwrap_or_default());
        }
    }
    crate::store::ops::batch_write(&sh2.store, batch).unwrap();
    // 0 < n < 5000 covers all 1200 rows -> over the cap -> SeqScan
    assert!(matches!(
        plan(
            &sh2.store,
            &wide,
            None,
            Some(&filter_of("SELECT 1 FROM t WHERE n BETWEEN 0 AND 5000"))
        ),
        Path::SeqScan
    ));
}
