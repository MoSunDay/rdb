//! Index-maintenance tests for [`crate::sql::tx::session`] commits
//! (split from session_tests.rs to respect the file-size budget).

use super::session_tests::{pk_key, seed_catalog, shared};
use super::*;
use crate::sql::parse::error::ErrorCode;

/// Schema with one secondary (v) + one unique (n) index for the
/// commit-maintenance tests.
fn indexed_schema(id: u32, name: &str) -> TableSchema {
    use crate::sql::storage::schema::{ColumnDef, IndexDef, SqlType};
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

#[tokio::test]
async fn commit_maintains_index_entries() {
    use crate::sql::index::{self, IndexRef};
    let s = indexed_schema(1, "t");
    let shared = shared();
    seed_catalog(&shared, &s);
    let oracle = &shared.sql_ts;

    let mut txn = begin(oracle);
    stage_upsert(
        &mut txn,
        &s,
        vec![Value::Int(1), Value::Str("a".into()), Value::Int(10)],
    )
    .unwrap();
    commit(&shared, txn).await.expect("commit");

    let iv = IndexRef::of(&s.indexes[0]);
    let un = IndexRef::of(&s.indexes[1]);
    assert_eq!(
        index::lookup_pks(&shared.store, &s, &iv, &Value::Str("a".into())).unwrap(),
        vec![pk_key(1)]
    );
    assert_eq!(
        index::unique_owner(&shared.store, &s, &un, &Value::Int(10)).unwrap(),
        Some(pk_key(1))
    );

    // staged UPDATE + DELETE maintain entries at COMMIT too
    let mut txn = begin(oracle);
    stage_upsert(
        &mut txn,
        &s,
        vec![Value::Int(1), Value::Str("b".into()), Value::Int(20)],
    )
    .unwrap();
    commit(&shared, txn).await.expect("commit");
    assert!(
        index::lookup_pks(&shared.store, &s, &iv, &Value::Str("a".into()))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        index::lookup_pks(&shared.store, &s, &iv, &Value::Str("b".into())).unwrap(),
        vec![pk_key(1)]
    );

    let mut txn = begin(oracle);
    stage_delete(&mut txn, &s, pk_key(1));
    commit(&shared, txn).await.expect("commit");
    assert!(
        index::lookup_pks(&shared.store, &s, &iv, &Value::Str("b".into()))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        index::unique_owner(&shared.store, &s, &un, &Value::Int(20)).unwrap(),
        None
    );
}

#[tokio::test]
async fn commit_rejects_unique_clash_against_committed_owner() {
    let s = indexed_schema(1, "t");
    let shared = shared();
    seed_catalog(&shared, &s);
    let oracle = &shared.sql_ts;

    // owner row enters through a real commit, so its unique entry exists
    let mut owner = begin(oracle);
    stage_upsert(
        &mut owner,
        &s,
        vec![Value::Int(1), Value::Null, Value::Int(7)],
    )
    .unwrap();
    commit(&shared, owner).await.expect("owner commit");

    let mut txn = begin(oracle); // sees the owner
    stage_upsert(
        &mut txn,
        &s,
        vec![Value::Int(2), Value::Null, Value::Int(7)],
    )
    .unwrap();
    let err = commit(&shared, txn).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);
    // the loser wrote nothing: row 2 never existed
    let visible = crate::sql::exec::scan::visible_rows(&shared.store, &s, oracle.now()).unwrap();
    assert_eq!(visible.len(), 1);
}
