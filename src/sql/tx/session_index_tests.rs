//! Index-maintenance tests for [`crate::sql::tx::session`] commits
//! (split from session_tests.rs to respect the file-size budget).

use super::session_tests::{pk_key, seed_catalog, shared};
use super::*;
use crate::sql::parse::error::ErrorCode;

/// Schema with one secondary (v) + one unique (n) index for the
/// commit-maintenance tests.
fn indexed_schema(id: u32, name: &str) -> TableSchema {
    use crate::sql::storage::schema::{ColumnDef, Engine, IndexDef, KeyModel, SqlType};
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
        pk: vec!["id".to_string()],
        auto_increment: None,
        engine: Engine::Row,
        key_model: KeyModel::MySql,
        distribution: None,
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
    stage_delete(&mut txn, &s, pk_key(1)).unwrap();
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

#[tokio::test]
async fn commit_fails_fast_when_only_the_index_plane_is_remote() {
    // Regression (W1 Part A): `dist::plan::build` routes unique
    // reservations and secondary ops by their OWN slots, so a txn whose
    // row slots are all local can still commit as 2PC when an index
    // plane hashes to a remote owner. The strict-reserve probe set must
    // include the index keys: with the ts authority unreachable such a
    // commit fails fast (1213, retryable) instead of stamping
    // GAP-fallback versions through 2PC.
    use crate::sql::index::keys::index_slot;
    use crate::sql::tx::global::{ClusterTs, ClusterTsDeps};
    use crate::sql::tx::nodes::NodeBinds;
    use crate::topology;

    let mut shared = shared();
    // Ready 3-node world, this node = "b:2": slots 0..=5461 on a:1,
    // 5462..=10922 local, 10923..=16383 on c:3.
    *shared.topology.write().unwrap() = topology::refresh("a:1,b:2,c:3");
    shared.conf.bind = "b:2".to_string();
    // Pick a table id whose v-index plane is remote while some row pk
    // hashes local (row and index planes hash different inputs).
    let (table_id, pk) = (1..=64u32)
        .filter_map(|id| {
            let idx_remote = crate::sql::dist::any_remote_owner(
                &shared,
                &[crate::store::rocksdb::slot_prefix(index_slot(id, 1))],
            );
            let local_pk = (0..64i64).find(|&pk| {
                !crate::sql::dist::any_remote_owner(
                    &shared,
                    &[crate::sql::dist::row_probe(id, &pk_key(pk))],
                )
            });
            (idx_remote && local_pk.is_some()).then(|| (id, local_pk.unwrap()))
        })
        .next()
        .expect("some table id splits row-local/index-remote");
    let s = indexed_schema(table_id, "t");
    seed_catalog(&shared, &s);
    // Follower whose leader resolves to no http address in sql_nodes:
    // every block fetch fails, so the ts authority is unreachable.
    {
        let mut r = shared.raft.write().unwrap();
        r.is_leader = false;
        r.leader_addr = "raft-x".to_string();
    }
    shared
        .sql_ts
        .enable_cluster(std::sync::Arc::new(ClusterTs::new(ClusterTsDeps {
            raft: std::sync::Arc::clone(&shared.raft),
            topo: std::sync::Arc::clone(&shared.topology),
            binds: NodeBinds {
                resp: "b:2".to_string(),
                raft: "raft-b".to_string(),
                http: "http-b".to_string(),
                mysql: String::new(),
                sql_rpc: String::new(),
            },
            token: "tok".to_string(),
        })));

    let oracle = &shared.sql_ts;
    let mut txn = begin(oracle);
    stage_upsert(
        &mut txn,
        &s,
        vec![Value::Int(pk), Value::Str("a".into()), Value::Int(10)],
    )
    .unwrap();
    let err = commit(&shared, txn).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::WriteConflict);
    assert!(
        err.msg.contains("ts authority unreachable"),
        "strict reserve error, got: {}",
        err.msg
    );
    // The veto fired before any alloc or write: nothing was stamped.
    let visible = crate::sql::exec::scan::visible_rows(&shared.store, &s, oracle.now()).unwrap();
    assert!(visible.is_empty());
}
