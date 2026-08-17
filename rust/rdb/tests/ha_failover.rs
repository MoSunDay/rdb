//! Integration check of the HA failover handler (P2 TODO 9): a single
//! bootstrapped raft node seeds `backup_target_map_<peer>` and
//! `cluster_slots_stable_instances` through `client_write`, then
//! `handler_observer` must swap the failed node for its backup and back.
//! (The pure string-replacement core is covered by ha.rs unit tests.)

use std::time::{Duration, Instant};

use rdb::rcache::fsm::KvMap;
use rdb::rcache::{ha, new_raft_node, transport, RaftNode};
use rdb::rtypes::RaftLogEntryData;

const INSTANCES_KEY: &str = "cluster_slots_stable_instances";

/// Poll the live FSM map until `key` maps to `value`.
async fn wait_for_kv(kv: &KvMap, key: &str, value: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if kv.read().unwrap().get(key).map(String::as_str) == Some(value) {
            return;
        }
        assert!(Instant::now() < deadline, "FSM never applied {key}={value}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_observer_fails_over_and_back() {
    std::env::set_var("RAFT_BOOTSTRAP", "true");

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("raft");
    let tcp_addr = "127.0.0.1:22901";
    let RaftNode { raft, kv } = new_raft_node(data_dir.to_str().unwrap(), tcp_addr)
        .await
        .unwrap();
    std::env::remove_var("RAFT_BOOTSTRAP");

    // Single voter: wait until bootstrap leadership is established.
    raft.wait(Some(Duration::from_secs(10)))
        .current_leader(transport::node_id_of(tcp_addr), "bootstrap node must lead")
        .await
        .unwrap();

    // Seed the failed peer's backup map and the instances list.
    let peer = "127.0.0.1:22681";
    raft.client_write(RaftLogEntryData {
        key: format!("backup_target_map_{peer}"),
        value: "127.0.0.1:32681,127.0.0.1:32684".into(),
    })
    .await
    .unwrap();
    raft.client_write(RaftLogEntryData {
        key: INSTANCES_KEY.into(),
        value: "127.0.0.1:32681,127.0.0.1:32683".into(),
    })
    .await
    .unwrap();
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;

    // Failed observation: src is replaced by its backup target.
    ha::handler_observer(raft.clone(), kv.clone(), "FailedHeartbeatObservation", peer).await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32684,127.0.0.1:32683").await;

    // Resumed observation: target is swapped back to src.
    ha::handler_observer(
        raft.clone(),
        kv.clone(),
        "ResumedHeartbeatObservation",
        peer,
    )
    .await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;

    // Resumed again with src already present: Go returns silently and
    // the instances list must stay untouched.
    ha::handler_observer(
        raft.clone(),
        kv.clone(),
        "ResumedHeartbeatObservation",
        peer,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;
}
