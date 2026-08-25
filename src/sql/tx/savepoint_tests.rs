//! SAVEPOINT / ROLLBACK TO / RELEASE SAVEPOINT tests on the staged
//! writes [`Txn`] (sibling of session_tests so every file stays inside
//! its line budget).

use super::session_tests::{seed_catalog, shared};
use super::*;
use crate::sql::parse::error::ErrorCode;
use crate::sql::storage::schema::Value;

fn schema(id: u32, name: &str) -> TableSchema {
    use crate::sql::storage::schema::{ColumnDef, Engine, SqlType};
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
        engine: Engine::Row,
        indexes: vec![],
        auto_increment: None,
    }
}

fn row_of(pk: i64, v: &str) -> Vec<Value> {
    vec![Value::Int(pk), Value::Str(v.to_string())]
}

fn stage(txn: &mut Txn, s: &TableSchema, pk: i64, v: &str) {
    stage_upsert(txn, s, row_of(pk, v)).unwrap();
}

/// (pk, v) of every staged row, in pk order -- readable assertions.
fn staged(txn: &Txn) -> Vec<(i64, String)> {
    txn.writes
        .values()
        .map(|w| match w {
            TxnWrite::Row(values) => (
                match &values[0] {
                    Value::Int(n) => *n,
                    other => panic!("pk not int: {other:?}"),
                },
                match &values[1] {
                    Value::Str(v) => v.clone(),
                    other => panic!("v not str: {other:?}"),
                },
            ),
            TxnWrite::Tombstone => panic!("tombstone unexpected"),
        })
        .collect()
}

#[test]
fn savepoint_rollback_restores_pre_marker_state() {
    let s = schema(40, "sp");
    let shared = shared();
    let mut txn = begin(&shared.sql_ts);
    stage(&mut txn, &s, 1, "before");
    savepoint(&mut txn, "sp1");
    stage(&mut txn, &s, 2, "after");
    stage(&mut txn, &s, 1, "overwritten-after"); // overwrites pk 1 in place
    assert_eq!(
        txn.writes.len(),
        2,
        "one entry per pk: the re-stage collapsed"
    );

    rollback_to(&mut txn, "sp1").unwrap();
    // the write set is EXACTLY the pre-marker one: the post-marker
    // insert is gone AND pk 1's pre-marker value is restored (a bare
    // length-truncation could not do that).
    assert_eq!(staged(&txn), vec![(1, "before".to_string())]);
    // the savepoint itself stays active: rolling back to it again is
    // legal and idempotent.
    assert_eq!(txn.savepoints.len(), 1);
    rollback_to(&mut txn, "sp1").unwrap();
    assert_eq!(staged(&txn), vec![(1, "before".to_string())]);

    rollback(&shared.sql_ts, txn);
}

#[test]
fn savepoint_name_shadowing_uses_latest_marker() {
    let s = schema(41, "shadow");
    let shared = shared();
    let mut txn = begin(&shared.sql_ts);
    stage(&mut txn, &s, 1, "first");
    savepoint(&mut txn, "sp");
    stage(&mut txn, &s, 2, "second");
    // reusing the name shadows the old marker (MySQL allows this).
    savepoint(&mut txn, "sp");
    stage(&mut txn, &s, 3, "third");

    rollback_to(&mut txn, "sp").unwrap();
    assert_eq!(
        staged(&txn),
        vec![(1, "first".to_string()), (2, "second".to_string())]
    );

    rollback(&shared.sql_ts, txn);
}

#[test]
fn rollback_to_drops_later_savepoints() {
    let shared = shared();
    let mut txn = begin(&shared.sql_ts);
    savepoint(&mut txn, "a");
    savepoint(&mut txn, "b");
    rollback_to(&mut txn, "a").unwrap();
    let err = rollback_to(&mut txn, "b").expect_err("marker dropped");
    assert_eq!(err.code, ErrorCode::UnknownSavepoint, "{err:?}");
    assert!(err.msg.contains("SAVEPOINT b does not exist"), "{err:?}");
    // "a" itself survives ROLLBACK TO (the target is kept).
    rollback_to(&mut txn, "a").unwrap();

    rollback(&shared.sql_ts, txn);
}

#[test]
fn release_savepoint_removes_marker_and_later_ones() {
    let s = schema(43, "rel");
    let shared = shared();
    let mut txn = begin(&shared.sql_ts);
    stage(&mut txn, &s, 1, "kept");
    savepoint(&mut txn, "a");
    stage(&mut txn, &s, 2, "also-kept");
    savepoint(&mut txn, "b");
    // RELEASE undoes nothing: staged writes survive.
    release_savepoint(&mut txn, "a").unwrap();
    assert!(txn.savepoints.is_empty());
    assert_eq!(
        staged(&txn),
        vec![(1, "kept".to_string()), (2, "also-kept".to_string())]
    );
    for name in ["a", "b"] {
        let err = rollback_to(&mut txn, name).expect_err("markers gone");
        assert_eq!(err.code, ErrorCode::UnknownSavepoint);
    }

    rollback(&shared.sql_ts, txn);
}

#[test]
fn unknown_savepoint_is_error_1305_style() {
    let shared = shared();
    let mut txn = begin(&shared.sql_ts);
    for err in [
        rollback_to(&mut txn, "nope").expect_err("never set"),
        release_savepoint(&mut txn, "nope").expect_err("never set"),
    ] {
        assert_eq!(err.code, ErrorCode::UnknownSavepoint, "{err:?}");
        assert!(err.msg.contains("SAVEPOINT nope does not exist"), "{err:?}");
    }

    rollback(&shared.sql_ts, txn);
}

#[tokio::test]
async fn savepoints_do_not_survive_commit_or_rollback() {
    let s = schema(45, "clear");
    let shared = shared();
    seed_catalog(&shared, &s);
    // ROLLBACK consumes the whole txn, savepoints included.
    let mut a = begin(&shared.sql_ts);
    savepoint(&mut a, "sp");
    rollback(&shared.sql_ts, a);
    // COMMIT: same consumption, on the success path.
    let mut b = begin(&shared.sql_ts);
    stage(&mut b, &s, 1, "gone");
    savepoint(&mut b, "sp");
    commit(&shared, b).await.unwrap();

    let mut fresh = begin(&shared.sql_ts);
    let err = rollback_to(&mut fresh, "sp").expect_err("cleared");
    assert_eq!(err.code, ErrorCode::UnknownSavepoint);
    assert_eq!(fresh.writes.len(), 0, "nothing from prior txns");
    rollback(&shared.sql_ts, fresh);
}

#[tokio::test]
async fn staged_write_visible_inside_txn_until_rollback_to() {
    let s = schema(46, "vis");
    let shared = shared();
    seed_catalog(&shared, &s);
    let mut txn = begin(&shared.sql_ts);
    assert!(merge_rows(&s, Vec::new(), &txn).unwrap().is_empty());

    stage(&mut txn, &s, 1, "first");
    savepoint(&mut txn, "sp");
    stage(&mut txn, &s, 2, "second");
    let merged = merge_rows(&s, Vec::new(), &txn).unwrap();
    assert_eq!(merged.len(), 2, "own staged writes visible: {merged:?}");

    rollback_to(&mut txn, "sp").unwrap();
    let merged = merge_rows(&s, Vec::new(), &txn).unwrap();
    assert_eq!(merged, vec![row_of(1, "first")]);
    rollback(&shared.sql_ts, txn);
}

#[test]
fn rollback_to_keeps_pre_savepoint_latches_releases_later_ones() {
    use crate::sql::tx::latch;
    let shared = shared();
    let mut txn = begin(&shared.sql_ts);
    latch::acquire(txn.id, &[(1, b"pre".to_vec())], true).unwrap();
    savepoint(&mut txn, "sp");
    latch::acquire(
        txn.id,
        &[(1, b"post1".to_vec()), (1, b"post2".to_vec())],
        true,
    )
    .unwrap();
    assert_eq!(latch::held_by(txn.id).len(), 3);

    // MySQL keeps row locks acquired BEFORE the savepoint; the ones
    // taken after it are released with the undone writes.
    rollback_to(&mut txn, "sp").unwrap();
    assert_eq!(latch::held_by(txn.id), vec![(1, b"pre".to_vec())]);

    // COMMIT/ROLLBACK releases the rest.
    let owner = txn.id;
    rollback(&shared.sql_ts, txn);
    assert!(latch::held_by(owner).is_empty());
}
