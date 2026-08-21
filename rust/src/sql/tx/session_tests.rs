//! Unit tests for [`crate::sql::tx::session`] (sibling file so
//! session.rs stays under the 400-line budget for new files).

use super::*;
use crate::sql::parse::error::ErrorCode;
use crate::state::testutil;

fn schema(id: u32, name: &str) -> TableSchema {
    use crate::sql::storage::schema::{ColumnDef, SqlType};
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

pub(super) fn shared() -> Shared {
    testutil::shared_with(testutil::test_config())
}

/// Publish schemas into the stub raft catalog (what DDL would do).
pub(super) fn seed_catalog(shared: &Shared, s: &TableSchema) {
    shared.raft.write().unwrap().kv.insert(
        crate::sql::storage::catalog::catalog_key(&s.name),
        serde_json::to_string(s).unwrap(),
    );
}

fn row_of(pk: i64, v: &str) -> Vec<Value> {
    vec![Value::Int(pk), Value::Str(v.to_string())]
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

pub(super) fn pk_key(pk: i64) -> Vec<u8> {
    row::pk_encode(&Value::Int(pk)).unwrap()
}

#[test]
fn begin_pins_latest_ts_and_registers_snapshot() {
    let shared = shared();
    let oracle = &shared.sql_ts;
    assert_eq!(oracle.now(), 0);
    let txn = begin(oracle);
    assert_eq!(txn.read_ts, 0);
    assert!(txn.writes.is_empty());
    oracle.alloc_n(10);
    assert_eq!(oracle.watermark(), 0, "open snapshot pins the watermark");
    oracle.unregister_snapshot(txn.read_ts);
    assert_eq!(oracle.watermark(), 10);
}

#[test]
fn shared_read_ts_is_refcounted() {
    let shared = shared();
    let oracle = &shared.sql_ts;
    let a = begin(oracle);
    let b = begin(oracle); // same read_ts: no write happened in between
    assert_eq!(a.read_ts, b.read_ts);
    rollback(oracle, a);
    assert_eq!(oracle.watermark(), b.read_ts, "second holder keeps the pin");
    rollback(oracle, b);
    assert_eq!(oracle.watermark(), oracle.now());
}

#[test]
fn staging_collapses_to_last_write_per_pk() {
    let s = schema(1, "t");
    let mut txn = Txn::default();
    stage_upsert(&mut txn, &s, row_of(1, "first")).unwrap();
    stage_upsert(&mut txn, &s, row_of(1, "second")).unwrap();
    assert_eq!(
        txn.writes.get(&(1, pk_key(1))),
        Some(&TxnWrite::Row(row_of(1, "second")))
    );
    stage_delete(&mut txn, &s, pk_key(1));
    assert_eq!(txn.writes.get(&(1, pk_key(1))), Some(&TxnWrite::Tombstone));
    assert_eq!(txn.writes.len(), 1);
}

#[test]
fn merge_rows_substitutes_deletes_and_injects() {
    let s = schema(1, "t");
    let shared = shared();
    put_version(&shared, &s, 1, 1, Some(row_of(1, "one")));
    put_version(&shared, &s, 2, 1, Some(row_of(2, "two")));
    put_version(&shared, &s, 3, 1, Some(row_of(3, "three")));
    let store_rows = crate::sql::exec::scan::visible_rows(&shared.store, &s, 1).unwrap();

    let mut txn = Txn {
        read_ts: 1,
        writes: BTreeMap::new(),
    };
    stage_upsert(&mut txn, &s, row_of(2, "TWO")).unwrap();
    stage_delete(&mut txn, &s, pk_key(3));
    stage_upsert(&mut txn, &s, row_of(5, "five")).unwrap();

    assert_eq!(
        merge_rows(&s, store_rows, &txn).unwrap(),
        vec![row_of(1, "one"), row_of(2, "TWO"), row_of(5, "five")]
    );
}

#[test]
fn merge_rows_reinserts_tombstoned_store_key() {
    let s = schema(1, "t");
    let shared = shared();
    put_version(&shared, &s, 7, 1, Some(row_of(7, "old")));
    put_version(&shared, &s, 7, 2, None); // deleted before our snapshot
    let store_rows = crate::sql::exec::scan::visible_rows(&shared.store, &s, 5).unwrap();
    assert!(store_rows.is_empty());

    let mut txn = Txn {
        read_ts: 5,
        writes: BTreeMap::new(),
    };
    stage_upsert(&mut txn, &s, row_of(7, "new")).unwrap();
    assert_eq!(
        merge_rows(&s, store_rows, &txn).unwrap(),
        vec![row_of(7, "new")]
    );
}

#[test]
fn merge_rows_ignores_other_tables() {
    let a = schema(1, "a");
    let b = schema(2, "b");
    let mut txn = Txn {
        read_ts: 0,
        writes: BTreeMap::new(),
    };
    stage_upsert(&mut txn, &b, row_of(9, "x")).unwrap();
    assert_eq!(
        merge_rows(&a, vec![row_of(1, "one")], &txn).unwrap(),
        vec![row_of(1, "one")]
    );
}

#[test]
fn conflict_check_first_committer_wins() {
    let s = schema(1, "t");
    let shared = shared();
    put_version(&shared, &s, 1, 1, Some(row_of(1, "seed")));

    let mut stale = Txn {
        read_ts: 1,
        writes: BTreeMap::new(),
    };
    stage_upsert(&mut stale, &s, row_of(1, "stale")).unwrap();
    conflict_check(&shared.store, &stale).expect("no newer version yet");

    put_version(&shared, &s, 1, 2, Some(row_of(1, "winner")));
    let err = conflict_check(&shared.store, &stale).unwrap_err();
    assert_eq!(err.code, ErrorCode::WriteConflict);
    assert!(err.msg.contains("write-write conflict on PK"), "{err}");

    // a fresh txn reading at now() sees the winner and validates fine
    let mut fresh = Txn {
        read_ts: 2,
        writes: BTreeMap::new(),
    };
    stage_upsert(&mut fresh, &s, row_of(1, "next")).unwrap();
    conflict_check(&shared.store, &fresh).expect("read_ts covers the winner");
}

#[test]
fn conflict_check_missing_pk_is_clean() {
    let s = schema(1, "t");
    let shared = shared();
    let mut txn = Txn {
        read_ts: 0,
        writes: BTreeMap::new(),
    };
    stage_upsert(&mut txn, &s, row_of(42, "fresh insert")).unwrap();
    conflict_check(&shared.store, &txn).expect("no prior version exists");
}

#[test]
fn build_commit_batch_assigns_sequential_ts_in_key_order() {
    let s1 = schema(1, "a");
    let s2 = schema(2, "b");
    let mut writes = BTreeMap::new();
    writes.insert((1, pk_key(2)), TxnWrite::Row(row_of(2, "a2")));
    writes.insert((1, pk_key(1)), TxnWrite::Row(row_of(1, "a1")));
    writes.insert((2, pk_key(1)), TxnWrite::Tombstone);
    let mut schemas = BTreeMap::new();
    schemas.insert(1u32, s1.clone());
    schemas.insert(2u32, s2.clone());

    let shared = shared();
    let batch = build_commit_batch(&writes, &schemas, 100..103).unwrap();
    assert_eq!(batch.len(), 3);
    ops::batch_write(&shared.store, batch).unwrap();

    // (table_id, pk_key, ts, decoded payload) of every staged version,
    // in (table_id, pk) order -- the same order ts was assigned in.
    type Staged = (u32, Vec<u8>, u64, Option<Vec<Value>>);
    let mut seen: Vec<Staged> = Vec::new();
    ops::for_each_from(&shared.store, b"0/", false, &mut |key, val| {
        if let Some((_, table_id, pk, ts)) = row::parse_version_key(key) {
            let s = &schemas[&table_id];
            let (header, values) = row::decode_version(s, val).unwrap();
            seen.push((
                table_id,
                pk,
                ts,
                (header == row::HEADER_LIVE).then_some(values),
            ));
        }
        true
    })
    .unwrap();
    seen.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    let key1 = pk_key(1);
    let key2 = pk_key(2);
    assert_eq!(seen[0], (1, key1.clone(), 100, Some(row_of(1, "a1"))));
    assert_eq!(seen[1], (1, key2.clone(), 101, Some(row_of(2, "a2"))));
    assert_eq!(seen[2], (2, key1.clone(), 102, None));
}

#[tokio::test]
async fn commit_persists_versions_and_releases_snapshot() {
    let s = schema(1, "t");
    let shared = shared();
    seed_catalog(&shared, &s);
    let oracle = &shared.sql_ts;
    let mut txn = begin(oracle);
    stage_upsert(&mut txn, &s, row_of(1, "one")).unwrap();
    stage_delete(&mut txn, &s, pk_key(2));

    commit(&shared, txn).await.expect("commit");
    assert_eq!(oracle.watermark(), oracle.now(), "snapshot released");
    let visible = crate::sql::exec::scan::visible_rows(&shared.store, &s, oracle.now()).unwrap();
    assert_eq!(visible, vec![row_of(1, "one")]);
}
#[tokio::test]
async fn rollback_discards_writes_and_releases_snapshot() {
    let s = schema(1, "t");
    let shared = shared();
    let oracle = &shared.sql_ts;
    let mut txn = begin(oracle);
    stage_upsert(&mut txn, &s, row_of(1, "one")).unwrap();
    rollback(oracle, txn);

    assert_eq!(oracle.watermark(), oracle.now());
    let visible = crate::sql::exec::scan::visible_rows(&shared.store, &s, oracle.now()).unwrap();
    assert!(visible.is_empty(), "nothing staged ever reached the store");
}

#[tokio::test]
async fn conflicting_commit_releases_snapshot_and_writes_nothing() {
    let s = schema(1, "t");
    let shared = shared();
    seed_catalog(&shared, &s);
    let oracle = &shared.sql_ts;
    oracle.alloc_n(1); // ts 1: the seed write
    put_version(&shared, &s, 1, 1, Some(row_of(1, "seed")));

    let mut txn = begin(oracle); // read_ts = 1
    stage_upsert(&mut txn, &s, row_of(1, "loser")).unwrap();
    oracle.alloc_n(1); // ts 2: someone else commits first
    put_version(&shared, &s, 1, 2, Some(row_of(1, "winner")));

    let err = commit(&shared, txn).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::WriteConflict);
    assert_eq!(
        oracle.watermark(),
        oracle.now(),
        "snapshot released on error"
    );
    let visible = crate::sql::exec::scan::visible_rows(&shared.store, &s, oracle.now()).unwrap();
    assert_eq!(visible, vec![row_of(1, "winner")], "loser wrote nothing");
}
