//! Unit tests of the index layer: pure op derivation, store lookups,
//! unique rejection, DROP sweeps. Store-backed cases use the tempdir
//! `testutil::shared_with` world, mirroring gc.rs.

use super::*;
use crate::sql::parse::error::ErrorCode;
use crate::sql::storage::row;
use crate::sql::storage::schema::{ColumnDef, IndexDef};
use crate::state::{testutil, Shared};
use std::sync::Arc;

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
        indexes: vec![
            IndexDef {
                id: 1,
                name: "idx_v".into(),
                column: "v".into(),
                unique: false,
            },
            IndexDef {
                id: 2,
                name: "uq_n".into(),
                column: "n".into(),
                unique: true,
            },
        ],
    }
}

fn idx_v(s: &TableSchema) -> IndexRef {
    IndexRef::of(&s.indexes[0])
}

fn uq_n(s: &TableSchema) -> IndexRef {
    IndexRef::of(&s.indexes[1])
}

/// Side of a row transition, borrowing caller-owned buffers.
fn side<'a>(pk_key: &'a [u8], values: &'a [Value]) -> RowSide<'a> {
    RowSide { pk_key, values }
}

fn pk_of(pk: i64) -> Vec<u8> {
    row::pk_encode(&Value::Int(pk)).unwrap()
}

/// Index entries for a fresh row, borrowing locals the caller owns.
fn insert_ops<'r>(
    s: &TableSchema,
    index: &IndexRef,
    pk_key: &'r [u8],
    values: &'r [Value],
) -> IndexOps {
    entries_for_row(s, index, None, Some(side(pk_key, values))).unwrap()
}

fn put_rows(shared: &Shared, s: &TableSchema, ts: u64, rows: &[Vec<Value>]) {
    let mut batch = rocksdb::WriteBatch::default();
    for r in rows {
        let pk_key = row::pk_encode(&r[s.pk_index()]).unwrap();
        batch.put(
            row::version_key(s, row::row_slot(s, &pk_key), &pk_key, ts),
            row::encode_row(s, r).unwrap(),
        );
    }
    ops::batch_write(&shared.store, batch).unwrap();
}

/// Stage rows + their entries for EVERY index of the schema on disk at
/// ts, so later checks read a populated index.
fn seed(shared: &Shared, s: &TableSchema, ts: u64, rows: &[Vec<Value>]) {
    put_rows(shared, s, ts, rows);
    let pk_keys: Vec<Vec<u8>> = rows
        .iter()
        .map(|r| row::pk_encode(&r[s.pk_index()]).unwrap())
        .collect();
    let mut ops = IndexOps::new();
    for def in &s.indexes {
        let index = IndexRef::of(def);
        for (r, pk) in rows.iter().zip(pk_keys.iter()) {
            ops.extend(insert_ops(s, &index, pk, r));
        }
    }
    let mut batch = rocksdb::WriteBatch::default();
    maintain::apply_ops(&mut batch, ops);
    ops::batch_write(&shared.store, batch).unwrap();
}

#[test]
fn entries_insert_update_delete() {
    let s = schema();
    let iv = idx_v(&s);
    let pk = pk_of(1);
    // insert: one put keyed by (value, pk), empty value
    let red = [Value::Int(1), Value::Str("red".into()), Value::Null];
    let ops = insert_ops(&s, &iv, &pk, &red);
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].1, Some(Vec::new()));
    assert_eq!(
        ops[0].0,
        keys::secondary_key(
            s.id,
            1,
            &keys::col_key_of(&Value::Str("red".into())).unwrap(),
            &pk
        )
    );
    // update, unindexed columns only -> nothing
    let o = [Value::Int(1), Value::Str("red".into()), Value::Int(5)];
    let n = [Value::Int(1), Value::Str("red".into()), Value::Int(6)];
    let same = entries_for_row(&s, &iv, Some(side(&pk, &o)), Some(side(&pk, &n))).unwrap();
    assert!(same.is_empty());
    // update, indexed col change -> delete old entry + put new one
    let n2 = [Value::Int(1), Value::Str("blue".into()), Value::Int(6)];
    let ops = entries_for_row(&s, &iv, Some(side(&pk, &o)), Some(side(&pk, &n2))).unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].1, None); // delete red/pk
    assert_eq!(ops[1].1, Some(vec![])); // put blue/pk
                                        // delete -> one delete
    let ops = entries_for_row(&s, &iv, Some(side(&pk, &o)), None).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].1, None);
}

#[test]
fn null_values_are_never_indexed() {
    let s = schema();
    let pk = pk_of(1);
    let nulls = [Value::Int(1), Value::Null, Value::Null];
    assert!(insert_ops(&s, &idx_v(&s), &pk, &nulls).is_empty());
    assert!(insert_ops(&s, &uq_n(&s), &pk, &nulls).is_empty());
}

#[test]
fn unique_entries_carry_the_pk_as_value() {
    let s = schema();
    let un = uq_n(&s);
    let pk = pk_of(3);
    let r = [Value::Int(3), Value::Str("x".into()), Value::Int(30)];
    let ops = insert_ops(&s, &un, &pk, &r);
    assert_eq!(ops.len(), 1);
    let ck = keys::col_key_of(&Value::Int(30)).unwrap();
    assert_eq!(ops[0].0, keys::unique_key(s.id, 2, &ck));
    assert_eq!(ops[0].1, Some(pk.clone()));
}

#[test]
fn lookup_and_unique_owner_over_store() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = schema();
    let iv = idx_v(&s);
    let un = uq_n(&s);
    let rows = vec![
        vec![Value::Int(1), Value::Str("red".into()), Value::Int(10)],
        vec![Value::Int(2), Value::Str("red".into()), Value::Int(20)],
        vec![Value::Int(3), Value::Str("blue".into()), Value::Null],
    ];
    seed(&shared, &s, 1, &rows);

    let mut pks = lookup_pks(&shared.store, &s, &iv, &Value::Str("red".into())).unwrap();
    pks.sort();
    assert_eq!(pks, vec![pk_of(1), pk_of(2)]);
    assert!(
        lookup_pks(&shared.store, &s, &iv, &Value::Str("nope".into()))
            .unwrap()
            .is_empty()
    );
    assert!(lookup_pks(&shared.store, &s, &iv, &Value::Null)
        .unwrap()
        .is_empty());
    // range over strings: a..=red covers all three indexed rows
    let mut pks = lookup_range(
        &shared.store,
        &s,
        &iv,
        &Value::Str("a".into()),
        &Value::Str("red".into()),
    )
    .unwrap();
    pks.sort();
    assert_eq!(pks.len(), 3);
    // unique point owner + range
    assert_eq!(
        unique_owner(&shared.store, &s, &un, &Value::Int(10)).unwrap(),
        Some(pk_of(1))
    );
    assert_eq!(
        unique_owner(&shared.store, &s, &un, &Value::Int(99)).unwrap(),
        None
    );
    let pks = lookup_range(&shared.store, &s, &un, &Value::Int(10), &Value::Int(20)).unwrap();
    assert_eq!(pks.len(), 2);
    // visible_row_at_pk: live at ts >= 1, absent before
    let pk1 = pk_of(1);
    assert_eq!(
        visible_row_at_pk(&shared.store, &s, &pk1, 1)
            .unwrap()
            .unwrap(),
        rows[0]
    );
    assert!(visible_row_at_pk(&shared.store, &s, &pk1, 0)
        .unwrap()
        .is_none());
}

#[test]
fn batch_ops_rejects_duplicate_unique_values() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = schema();
    // pre-existing owner of n=10 on disk
    let owner_row = [Value::Int(1), Value::Null, Value::Int(10)];
    let pk1 = pk_of(1);
    seed(&shared, &s, 1, &[owner_row.to_vec()]);

    // a different pk claiming 10 -> duplicate (disk point-get)
    let dup_row = [Value::Int(2), Value::Null, Value::Int(10)];
    let pk2 = pk_of(2);
    let err = maintain::batch_ops(
        &shared.store,
        &s,
        &[maintain::Transition::insert(side(&pk2, &dup_row))],
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);
    assert!(
        err.msg.contains("Duplicate entry 10 for key 'uq_n'"),
        "{}",
        err.msg
    );

    // intra-batch duplicate (two fresh rows, one batch)
    let a = [Value::Int(3), Value::Null, Value::Int(77)];
    let b = [Value::Int(4), Value::Null, Value::Int(77)];
    let err = maintain::batch_ops(
        &shared.store,
        &s,
        &[
            maintain::Transition::insert(side(&pk_of(3), &a)),
            maintain::Transition::insert(side(&pk_of(4), &b)),
        ],
    )
    .unwrap_err();
    assert!(err.msg.contains("Duplicate entry 77"), "{}", err.msg);

    // the SAME pk re-claiming its own value is legal (no-op rewrite)
    assert!(maintain::batch_ops(
        &shared.store,
        &s,
        &[maintain::Transition::insert(side(&pk1, &owner_row))]
    )
    .is_ok());

    // value swap inside one UPDATE batch: 1 vacates 10, 2 claims it
    let rows = vec![
        vec![Value::Int(1), Value::Null, Value::Int(10)],
        vec![Value::Int(2), Value::Null, Value::Int(20)],
    ];
    seed(&shared, &s, 2, &rows);
    let o1 = rows[0].clone();
    let o2 = rows[1].clone();
    let n1 = [Value::Int(1), Value::Null, Value::Int(20)];
    let n2 = [Value::Int(2), Value::Null, Value::Int(10)];
    let swap = vec![
        maintain::Transition {
            old: Some(side(&pk1, &o1)),
            new: Some(side(&pk1, &n1)),
        },
        maintain::Transition {
            old: Some(side(&pk2, &o2)),
            new: Some(side(&pk2, &n2)),
        },
    ];
    assert!(maintain::batch_ops(&shared.store, &s, &swap).is_ok());
}

#[tokio::test]
async fn drop_entries_sweeps_only_that_column() {
    let shared = testutil::shared_with(testutil::test_config());
    let s = schema();
    let iv = idx_v(&s);
    let un = uq_n(&s);
    let rows = vec![
        vec![Value::Int(1), Value::Str("red".into()), Value::Int(10)],
        vec![Value::Int(2), Value::Str("blue".into()), Value::Int(20)],
    ];
    // both columns indexed: the sweep must spare the neighbours
    seed(&shared, &s, 1, &rows);

    // drop idx_v (col pos 1): v entries gone, n entries + rows remain
    let (n, next) = drop_entries_page(&shared.store, s.id, 1, &keys::sweep_start(s.id, 1)).unwrap();
    assert!(n >= 2, "deleted {n}");
    assert!(!next.is_empty());
    assert!(
        lookup_pks(&shared.store, &s, &iv, &Value::Str("red".into()))
            .unwrap()
            .is_empty()
    );
    assert!(unique_owner(&shared.store, &s, &un, &Value::Int(10))
        .unwrap()
        .is_some());
    let pk1 = pk_of(1);
    assert!(visible_row_at_pk(&shared.store, &s, &pk1, 1)
        .unwrap()
        .is_some());

    // async full sweep of uq_n (col pos 2)
    let dropped = drop_entries(Arc::clone(&shared.store), s.id, 2)
        .await
        .unwrap();
    assert!(dropped >= 2, "dropped {dropped}");
    assert_eq!(
        unique_owner(&shared.store, &s, &un, &Value::Int(10)).unwrap(),
        None
    );
}
