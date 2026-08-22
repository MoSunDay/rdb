//! End-to-end check that rcache's store + fsm + transport + service all
//! interoperate: two in-process raft nodes on real TCP sockets replicate a
//! client write from node1 to node2's state machine.
//!
//! Node1 is initialized as a single voter, node2 joins as learner and is
//! then promoted to voter; the test overrides the production snapshot
//! policy to compact the log after every apply, so node2 catches up
//! through the install-snapshot path, exercising every RPC kind of the
//! transport.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use openraft::{Config, Raft, SnapshotPolicy};
use rdb::rcache::transport::node_id_of;
use rdb::rcache::{fsm, raft_config, service, store, transport, RdbRaft};
use rdb::rtypes::RaftLogEntryData;

type KvMap = Arc<RwLock<HashMap<String, String>>>;

/// Reserve an unused loopback port and return it as a RaftTCPAddress.
/// Reuse after drop matches the Go tests' fixed-port approach closely
/// enough; the window is negligible for a single test process.
async fn free_addr() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    drop(listener);
    addr
}

/// `raft_config()` with the snapshot policy overridden to snapshot after
/// every apply, so a joining node must catch up via install-snapshot
/// (production uses `LogsSinceLast(8192)` and would rarely snapshot here).
fn test_config() -> Arc<Config> {
    let mut cfg = (*raft_config()).clone();
    cfg.snapshot_policy = SnapshotPolicy::LogsSinceLast(1);
    Arc::new(cfg.validate().unwrap())
}

/// Build one raft node: rocksdb log store + fsm in a temp dir, raft
/// instance with the TCP transport, and its RPC server task.
/// Returns handles; the `TempDir` must outlive the node.
async fn build_node(addr: String) -> (tempfile::TempDir, Arc<RdbRaft>, KvMap) {
    let dir = tempfile::tempdir().unwrap();
    let log_store = store::open(dir.path()).unwrap();
    let state_machine = fsm::StateMachine::new(log_store.clone()).unwrap();
    let kv = state_machine.data.kv.clone();

    let id = node_id_of(&addr);
    let raft = Raft::new(
        id,
        test_config(),
        transport::new(id),
        log_store,
        state_machine,
    )
    .await
    .unwrap();
    let raft = Arc::new(raft);

    let server = raft.clone();
    let server_addr = addr.clone();
    tokio::spawn(async move {
        if let Err(e) = service::serve(server_addr, server).await {
            eprintln!("rcache raft rpc: serve {addr} failed: {e}");
        }
    });

    (dir, raft, kv)
}

/// Poll `kv` until `key` maps to `value` or the deadline passes.
async fn wait_for_kv(kv: &KvMap, key: &str, value: &str, deadline: Instant) {
    loop {
        if kv.read().unwrap().get(key).map(String::as_str) == Some(value) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "node2 FSM never applied {key}={value}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_replicate_client_write() {
    let addr1 = free_addr().await;
    let addr2 = free_addr().await;
    let id1 = node_id_of(&addr1);
    let id2 = node_id_of(&addr2);
    assert_ne!(id1, id2);

    // Node1: pristine cluster with itself as the only voter.
    let (_dir1, raft1, _kv1) = build_node(addr1.clone()).await;
    raft1
        .initialize(BTreeMap::from([(id1, addr1.clone())]))
        .await
        .unwrap();
    raft1
        .wait(Some(Duration::from_secs(10)))
        .current_leader(id1, "node1 must lead its single-voter cluster")
        .await
        .unwrap();

    // Node2 joins as learner (blocking until caught up), then becomes a
    // voter alongside node1.
    let (_dir2, raft2, kv2) = build_node(addr2.clone()).await;
    raft1.add_learner(id2, addr2.clone(), true).await.unwrap();
    raft1.change_membership([id1, id2], true).await.unwrap();

    // Replicate one client write through the leader.
    let resp = raft1
        .client_write(RaftLogEntryData {
            key: "store/set".to_string(),
            value: r#"{"slot":12}"#.to_string(),
        })
        .await
        .unwrap();
    raft2
        .wait(Some(Duration::from_secs(30)))
        .applied_index_at_least(Some(resp.log_id.index), "node2 applied the client write")
        .await
        .unwrap();

    // The log index may lead the state machine slightly; poll the FSM.
    wait_for_kv(
        &kv2,
        "store/set",
        r#"{"slot":12}"#,
        Instant::now() + Duration::from_secs(30),
    )
    .await;

    raft1.shutdown().await.unwrap();
    raft2.shutdown().await.unwrap();
}
