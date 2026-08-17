//! Scenarios B + C -- process-level failover and restart semantics against
//! the REAL rdb binary: SIGKILL the leader of a 3-node cluster, watch the
//! survivors elect a new leader, restart the corpse with the SAME config +
//! data dir (no RAFT_BOOTSTRAP, no RAFT_JOIN_ADDR) and prove it catches up
//! through raft replication; then a single-node full restart proving the
//! rocksdb store survives SIGKILL.

mod common;

use std::time::{Duration, Instant};

use common::{
    all_ctx, cluster_init, cmd_one_shot, spawn_node, start_cluster, wait_cluster_nodes_list_all,
    wait_leader, wait_resp_ready, ProcNode, TOKEN,
};

/// Poll `raft get <key>` on `node` until it returns exactly `+<want>`.
/// (`raft get` reads the node's live FSM, so it works on followers and
/// reflects catch-up the moment replicated entries are applied.)
async fn poll_raft_get(node: &ProcNode, key: &str, want: &str, secs: u64) {
    let want_reply = format!("+{want}").into_bytes();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(&node.resp, TOKEN, &[b"raft", b"get", key.as_bytes()]).await;
        if r == want_reply {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "raft get {key} never returned {want}\nlast reply={r:?}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Poll plain `GET <key>` until it returns the bulk frame `$N\r\n<payload>`
/// (topology resync is a 3s ticker after a restart; refused reads are
/// simply retried).
async fn poll_get_bulk(node: &ProcNode, key: &str, want: &str, secs: u64) {
    let want_reply = format!("${}\r\n{want}", want.len()).into_bytes();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(&node.resp, TOKEN, &[b"get", key.as_bytes()]).await;
        if r == want_reply {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "get {key} never returned {want}\nlast reply={r:?}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Scenario B: kill -9 the leader, survivors elect, the restarted old
/// leader catches up on writes it missed while dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill9_leader_new_leader_elects_and_old_leader_catches_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut nodes, old) = start_cluster(dir.path(), 3).await;

    // Control-plane write before the kill (replicated to every FSM).
    assert_eq!(
        cmd_one_shot(&nodes[old].resp, TOKEN, &[b"raft", b"set", b"k1", b"v1"]).await,
        b"+OK",
        "raft set k1 on the leader\n{}",
        nodes[old].ctx()
    );

    // SIGKILL, no graceful shutdown.
    nodes[old].kill_now();

    // Survivors (2 of 3 = quorum) elect a new leader; the corpse just
    // never answers, so polling the whole set is safe.
    let new_leader = wait_leader(&nodes, 60).await;
    assert_ne!(
        new_leader,
        old,
        "the SIGKILLed leader cannot still be the leader\n{}",
        all_ctx(&nodes)
    );

    // A write on the NEW leader (sending it to a follower would answer
    // "internal error err: not leader").
    assert_eq!(
        cmd_one_shot(
            &nodes[new_leader].resp,
            TOKEN,
            &[b"raft", b"set", b"k2", b"v2"]
        )
        .await,
        b"+OK",
        "raft set k2 on the new leader\n{}",
        nodes[new_leader].ctx()
    );

    // Restart the old leader: same config path + data dir, NO bootstrap,
    // NO join -- its raft state and address are unchanged, so the new
    // leader replicates to it.
    nodes[old].respawn();
    wait_resp_ready(&mut nodes[old], 60).await;

    // Catch-up proof: k2 was committed while this node was dead.
    poll_raft_get(&nodes[old], "k2", "v2", 60).await;
    // And what it already had before dying is still there.
    poll_raft_get(&nodes[old], "k1", "v1", 60).await;
}

/// Scenario C: single-node cluster, full process restart, rocksdb data
/// survives and stays writable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_restart_preserves_rocksdb_data() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Single node: bootstrap alone, wait leader, init with just itself.
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    let l = wait_leader(std::slice::from_ref(&node), 60).await;
    assert_eq!(l, 0, "the lone bootstrapped node must lead\n{}", node.ctx());
    let binds = vec![node.resp.clone()];
    cluster_init(&node, &binds).await;
    wait_cluster_nodes_list_all(std::slice::from_ref(&node), &binds, 30).await;

    // Data-plane write + read (writes are synchronous, SIGKILL-safe).
    assert_eq!(
        cmd_one_shot(&node.resp, TOKEN, &[b"set", b"pkey", b"pvalue"]).await,
        b"+OK",
        "set pkey\n{}",
        node.ctx()
    );
    assert_eq!(
        cmd_one_shot(&node.resp, TOKEN, &[b"get", b"pkey"]).await,
        b"$6\r\npvalue",
        "get pkey\n{}",
        node.ctx()
    );

    // SIGKILL + restart with the same config path + data dir.
    node.respawn();
    wait_resp_ready(&mut node, 60).await;

    // AUTH + poll: the store must still hold the key (the polling absorbs
    // the pre-topology-sync window).
    poll_get_bulk(&node, "pkey", "pvalue", 30).await;

    // Post-restart mutation works.
    assert_eq!(
        cmd_one_shot(&node.resp, TOKEN, &[b"del", b"pkey"]).await,
        b":1",
        "del pkey after restart\n{}",
        node.ctx()
    );
    assert_eq!(
        cmd_one_shot(&node.resp, TOKEN, &[b"get", b"pkey"]).await,
        b"$-1",
        "get after del is nil\n{}",
        node.ctx()
    );
}
