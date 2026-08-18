//! Integration check of the HA failover handler (P2 TODO 9): a single
//! bootstrapped raft node seeds `backup_target_map_<peer>` and
//! `cluster_slots_stable_instances` through `client_write`, then
//! `handler_observer` must swap the failed node for its backup and back.
//! (The pure string-replacement core is covered by ha.rs unit tests.)

use std::sync::Arc;
use std::time::{Duration, Instant};

use rdb::rcache::fsm::KvMap;
use rdb::rcache::{ha, new_raft_node, transport, RaftNode};
use rdb::rtypes::RaftLogEntryData;

const INSTANCES_KEY: &str = "cluster_slots_stable_instances";
const RET_FAILED: &str = "FailedHeartbeatObservation";

/// new_raft_node reads the process-wide RAFT_BOOTSTRAP env var, so the
/// bootstrap tests in this binary serialize on this lock (cargo runs
/// tests on parallel threads otherwise).
static BOOTSTRAP_MUX: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Boot a single-voter leader on a fresh data dir (env-gated bootstrap),
/// waiting until it leads.
async fn bootstrap_leader(tcp_addr: &str) -> (tempfile::TempDir, Arc<rdb::rcache::RdbRaft>, KvMap) {
    std::env::set_var("RAFT_BOOTSTRAP", "true");
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("raft");
    let RaftNode { raft, kv } = new_raft_node(data_dir.to_str().unwrap(), tcp_addr)
        .await
        .unwrap();
    std::env::remove_var("RAFT_BOOTSTRAP");

    raft.wait(Some(Duration::from_secs(10)))
        .current_leader(transport::node_id_of(tcp_addr), "bootstrap node must lead")
        .await
        .unwrap();
    (dir, raft, kv)
}

/// Apply one RaftLogEntryData through the leader.
async fn seed(raft: &rdb::rcache::RdbRaft, key: &str, value: &str) {
    raft.client_write(RaftLogEntryData {
        key: key.to_string(),
        value: value.to_string(),
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handler_observer_fails_over_and_back() {
    let _bootstrap = BOOTSTRAP_MUX.lock().await;
    let (_dir, raft, kv) = bootstrap_leader("127.0.0.1:22901").await;

    // Seed the failed peer's backup map and the instances list.
    let peer = "127.0.0.1:22681";
    seed(
        &raft,
        &format!("backup_target_map_{peer}"),
        "127.0.0.1:32681,127.0.0.1:32684",
    )
    .await;
    seed(&raft, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;

    let mux = ha::observer_mux();

    // Failed observation: src is replaced by its backup target.
    ha::handler_observer(raft.clone(), kv.clone(), RET_FAILED, peer, mux.clone()).await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32684,127.0.0.1:32683").await;

    // Resumed observation: target is swapped back to src.
    ha::handler_observer(
        raft.clone(),
        kv.clone(),
        "ResumedHeartbeatObservation",
        peer,
        mux.clone(),
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
        mux.clone(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;
}

/// A lock-free operator write (Go `cluster init` style: a direct
/// client_write on INSTANCES_KEY) landing while a FAILED observer is in
/// flight must survive: the observer's converge loop reads the LATEST
/// committed value and folds both effects into one value. Holding the
/// mux here pins the interleaving deterministically — the observer
/// cannot read until the operator write has committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_concurrent_with_operator_write_loses_nothing() {
    let _bootstrap = BOOTSTRAP_MUX.lock().await;
    let (_dir, raft, kv) = bootstrap_leader("127.0.0.1:22902").await;

    let peer = "127.0.0.1:22682";
    seed(
        &raft,
        &format!("backup_target_map_{peer}"),
        "127.0.0.1:32681,127.0.0.1:32684",
    )
    .await;
    seed(&raft, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;

    let mux = ha::observer_mux();
    // Observer spawned but pinned behind the lock we hold...
    let gate = mux.lock().await;
    let observer = tokio::spawn(ha::handler_observer(
        raft.clone(),
        kv.clone(),
        RET_FAILED,
        peer,
        mux.clone(),
    ));
    // ...while the operator adds a brand-new instance to the list.
    seed(
        &raft,
        INSTANCES_KEY,
        "127.0.0.1:32681,127.0.0.1:32683,127.0.0.1:32685",
    )
    .await;
    drop(gate);
    observer.await.unwrap();

    // Union of both effects: failover swap AND the operator's instance.
    wait_for_kv(
        &kv,
        INSTANCES_KEY,
        "127.0.0.1:32684,127.0.0.1:32683,127.0.0.1:32685",
    )
    .await;
}

/// Two FAILED observers racing for DIFFERENT peers must both land: the
/// mux serializes them and the converge loop re-reads after each apply,
/// so the second swap is computed from the first one's result. (Without
/// the fix both decide on the same stale snapshot and one blind write
/// clobbers the other: only one swap ever becomes visible.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_concurrent_observers_keep_both_swaps() {
    let _bootstrap = BOOTSTRAP_MUX.lock().await;
    let (_dir, raft, kv) = bootstrap_leader("127.0.0.1:22903").await;

    let peer_a = "127.0.0.1:22683";
    let peer_b = "127.0.0.1:22684";
    seed(
        &raft,
        &format!("backup_target_map_{peer_a}"),
        "127.0.0.1:32681,127.0.0.1:32686",
    )
    .await;
    seed(
        &raft,
        &format!("backup_target_map_{peer_b}"),
        "127.0.0.1:32683,127.0.0.1:32687",
    )
    .await;
    seed(&raft, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;
    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32681,127.0.0.1:32683").await;

    let mux = ha::observer_mux();
    let ((), ()) = tokio::join!(
        ha::handler_observer(raft.clone(), kv.clone(), RET_FAILED, peer_a, mux.clone()),
        ha::handler_observer(raft.clone(), kv.clone(), RET_FAILED, peer_b, mux.clone()),
    );

    wait_for_kv(&kv, INSTANCES_KEY, "127.0.0.1:32686,127.0.0.1:32687").await;
}
