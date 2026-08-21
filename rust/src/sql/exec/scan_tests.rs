//! Unit tests for [`crate::sql::exec::scan`] (kept in a sibling file so
//! scan.rs stays under the 400-line budget for new files).

use super::*;
use crate::sql::storage::schema::ColumnDef;
use crate::state::testutil;

fn schema(id: u32, name: &str) -> TableSchema {
    TableSchema {
        id,
        name: name.to_string(),
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
        ],
        pk: "id".into(),
        indexes: vec![],
    }
}

fn shared() -> Shared {
    testutil::shared_with(testutil::test_config())
}

fn seed_catalog(shared: &Shared, schema: &TableSchema) {
    shared.raft.write().unwrap().kv.insert(
        catalog::catalog_key(&schema.name),
        serde_json::to_string(schema).unwrap(),
    );
}

fn put_version(shared: &Shared, s: &TableSchema, pk: i64, ts: u64, row: Option<Vec<Value>>) {
    let key = row::pk_encode(&Value::Int(pk)).unwrap();
    let k = row::version_key(s, row::row_slot(s, &key), &key, ts);
    let v = match row {
        Some(values) => row::encode_row(s, &values).unwrap(),
        None => row::encode_tombstone(),
    };
    let mut batch = rocksdb::WriteBatch::default();
    batch.put(k, v);
    ops::batch_write(&shared.store, batch).unwrap();
}

fn row_of(pk: i64, v: &str) -> Vec<Value> {
    vec![Value::Int(pk), Value::Str(v.to_string())]
}

#[test]
fn visible_rows_snapshot_and_tombstones() {
    let shared = shared();
    let s = schema(1, "t");
    put_version(&shared, &s, 1, 5, Some(row_of(1, "v5")));
    put_version(&shared, &s, 1, 7, Some(row_of(1, "v7")));
    put_version(&shared, &s, 1, 9, None); // deleted at ts 9
    put_version(&shared, &s, 2, 6, Some(row_of(2, "b")));

    assert_eq!(
        visible_rows(&shared.store, &s, 5).unwrap(),
        vec![row_of(1, "v5")]
    );
    // ts 7: pk 2 (written at 6) is visible alongside pk 1's v7.
    assert_eq!(
        visible_rows(&shared.store, &s, 7).unwrap(),
        vec![row_of(1, "v7"), row_of(2, "b")]
    );
    assert_eq!(
        visible_rows(&shared.store, &s, 8).unwrap(),
        vec![row_of(1, "v7"), row_of(2, "b")]
    );
    // after the tombstone only pk 2 remains visible.
    assert_eq!(
        visible_rows(&shared.store, &s, 100).unwrap(),
        vec![row_of(2, "b")]
    );
}

#[test]
fn visible_rows_are_ordered_by_pk() {
    let shared = shared();
    let s = schema(1, "t");
    for pk in [3, 1, 2] {
        put_version(&shared, &s, pk, 1, Some(row_of(pk, "x")));
    }
    let rows = visible_rows(&shared.store, &s, 10).unwrap();
    let pks: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            Value::Int(i) => i,
            _ => panic!("pk"),
        })
        .collect();
    assert_eq!(pks, vec![1, 2, 3]);
}

#[test]
fn materialize_single_table_reads_catalog() {
    let shared = shared();
    let s = schema(7, "t");
    seed_catalog(&shared, &s);
    put_version(&shared, &s, 1, 1, Some(row_of(1, "a")));
    put_version(&shared, &s, 2, 2, Some(row_of(2, "b")));

    let src = materialize(
        &shared,
        &TableRef::Table {
            name: "t".into(),
            alias: Some("x".into()),
        },
        5,
        None,
        None,
    )
    .unwrap();
    assert_eq!(src.rows, vec![row_of(1, "a"), row_of(2, "b")]);
    assert_eq!(src.scope.sides.len(), 1);
    assert_eq!(src.scope.sides[0].qualifier, "x"); // alias wins
    assert_eq!(src.scope.row_width(), 2);
    assert_eq!(src.scope.resolve(None, "v"), Some(1));

    let missing = materialize(
        &shared,
        &TableRef::Table {
            name: "nope".into(),
            alias: None,
        },
        5,
        None,
        None,
    );
    assert!(missing.is_err());
}

#[test]
fn materialize_nested_loop_join_with_on() {
    let shared = shared();
    let u = schema(1, "u");
    let o = TableSchema {
        id: 2,
        name: "o".into(),
        columns: vec![ColumnDef {
            name: "uid".into(),
            sql_type: SqlType::Int,
            nullable: false,
        }],
        pk: "uid".into(),
        indexes: vec![],
    };
    seed_catalog(&shared, &u);
    seed_catalog(&shared, &o);
    put_version(&shared, &u, 1, 1, Some(row_of(1, "a")));
    put_version(&shared, &u, 2, 1, Some(row_of(2, "b")));
    put_version(&shared, &o, 1, 1, Some(vec![Value::Int(1)]));
    put_version(&shared, &o, 2, 1, Some(vec![Value::Int(1)]));

    // u.id = o.uid
    let parsed =
        crate::sql::parse::parse_statement("SELECT * FROM u JOIN o ON u.id = o.uid").unwrap();
    let crate::sql::parse::ast::Statement::Select(q) = parsed else {
        panic!("shape")
    };
    let src = materialize(&shared, &q.from, 10, None, None).unwrap();
    assert_eq!(src.rows.len(), 2); // both o-rows match u.id=1
    assert_eq!(src.scope.row_width(), 3);
    assert_eq!(src.scope.resolve(Some("u"), "id"), Some(0));
    assert_eq!(src.scope.resolve(Some("o"), "uid"), Some(2));

    // cross join (no ON) keeps the full product
    let cross = TableRef::Join {
        left: Box::new(TableRef::Table {
            name: "u".into(),
            alias: None,
        }),
        right: Box::new(TableRef::Table {
            name: "o".into(),
            alias: None,
        }),
        on: None,
    };
    let src = materialize(&shared, &cross, 10, None, None).unwrap();
    assert_eq!(src.rows.len(), 4);
}

#[test]
fn resolve_reports_ambiguity() {
    let s = schema(1, "t");
    let other = TableSchema {
        id: 2,
        name: "t2".into(),
        columns: s.columns.clone(),
        pk: "id".into(),
        indexes: vec![],
    };
    let scope = FromScope {
        sides: vec![table_side(&s, &None), {
            let mut side = table_side(&other, &None);
            side.offset = 2;
            side
        }],
    };
    // "id" exists in both sides -> ambiguous unqualified.
    let err = scope.resolve_checked(None, "id").unwrap_err();
    assert!(err.msg.contains("ambiguous column 'id'"), "{}", err.msg);
    // qualified resolves.
    assert_eq!(scope.resolve_checked(Some("t2"), "id").unwrap(), 2);
    // unknown in the qualified side.
    assert!(scope.resolve_checked(Some("t"), "zz").is_err());
    assert!(scope.resolve_checked(None, "zz").is_err());
    // alias qualifies: t2 AS b -> b.id resolves into the second side.
    let scope = FromScope {
        sides: vec![table_side(&s, &None), {
            let mut side = table_side(&other, &Some("b".into()));
            side.offset = 2;
            side
        }],
    };
    assert_eq!(scope.resolve_checked(Some("b"), "id").unwrap(), 2);
    assert_eq!(scope.resolve(Some("t2"), "v"), Some(3));
}
