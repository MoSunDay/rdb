//! Process-level e2e of the `backup_target_map` backup takeover: the HA
//! path where a SIGKILLed member's slot band is re-homed onto ANOTHER
//! live process's read-only backup listener (tests/ha_failover.rs covers
//! only the in-proc observer; the scrtips env wiring was never exercised).
//! Semantics pinned from src/rcache/ha.rs + src/topology.rs: the leader
//! probes each voter's raft addr every 5s (5s TCP-connect timeout); a
//! refused connect is a Failed observation -> the victim's resp addr is
//! replaced IN PLACE by the map target inside the raft key
//! `cluster_slots_stable_instances`, so after the 3s topology resync
//! survivors reply `-MOVED <slot> <target>`; a live probe again is a
//! Resumed observation -> the swap-back restores the original list. The
//! takeover listener serves reads from its own (separate) store and
//! rejects writes with -READONLY.

mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{
    all_ctx, cluster_init, cmd_one_shot, contains_bytes, spawn_node_backup,
    wait_cluster_nodes_list_all, wait_leader, wait_resp_ready, ProcNode, TOKEN,
};

/// Redis-standard replica error served by the backup listener's gate.
const READONLY_ERR: &[u8] = b"READONLY You can't write against a read only replica.";

/// Poll `args` on node `idx` until `ok(reply)` holds; returns the reply.
/// Panics with the whole cluster's stderr tails when `secs` elapse.
async fn poll_reply<F>(nodes: &[ProcNode], idx: usize, args: &[&[u8]], secs: u64, ok: F) -> Vec<u8>
where
    F: Fn(&[u8]) -> bool,
{
    let shown: Vec<String> = args
        .iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let last = cmd_one_shot(&nodes[idx].resp, TOKEN, args).await;
        if ok(&last) {
            return last;
        }
        assert!(
            Instant::now() < deadline,
            "n{idx} {shown:?} never satisfied within {secs}s; last={last:?}\n{}",
            all_ctx(nodes)
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Poll `args` against a bare address until the reply equals `want`
/// (used for the backup listener, which no ProcNode field points at).
async fn poll_addr(node: &ProcNode, addr: &str, args: &[&[u8]], want: &[u8], secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let last = cmd_one_shot(addr, TOKEN, args).await;
        if last == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{addr} {args:?} never replied {want:?} within {secs}s; last={last:?}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

/// Poll `raft get <key>` on `node` until it returns the bulk frame of
/// `want` (reads the node's live FSM, so it also proves catch-up).
async fn poll_raft_get(node: &ProcNode, key: &str, want: &str, secs: u64) {
    let want_reply = format!("${}\r\n{want}", want.len()).into_bytes();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(&node.resp, TOKEN, &[b"raft", b"get", key.as_bytes()]).await;
        if r == want_reply {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "raft get {key} never returned {want:?}; last={r:?}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// `CLUSTER KEYSLOT` -> u16.
async fn keyslot(node: &ProcNode, key: &[u8]) -> u16 {
    let r = cmd_one_shot(&node.resp, TOKEN, &[b"cluster", b"keyslot", key]).await;
    assert!(r.starts_with(b":"), "keyslot reply {r:?}\n{}", node.ctx());
    String::from_utf8_lossy(&r[1..])
        .parse()
        .unwrap_or_else(|_| panic!("keyslot not numeric: {r:?}"))
}

/// Equal-split owner index for `slot` on a 3-node cluster (per_node_slots
/// 5461; the last node absorbs the remainder past 10922).
fn band_owner(slot: u16) -> usize {
    if slot <= 5461 {
        0
    } else if slot <= 10922 {
        1
    } else {
        2
    }
}

/// A short key whose slot lands in band `owner` (brute force via
/// CLUSTER KEYSLOT on any live node).
async fn band_key(node: &ProcNode, owner: usize) -> Vec<u8> {
    for i in 0..200u32 {
        let k = format!("bk{i}").into_bytes();
        if band_owner(keyslot(node, &k).await) == owner {
            return k;
        }
    }
    panic!("no short key landed in band {owner}\n{}", node.ctx());
}

/// Append the FULL cyclic `backup_target_map` block (reference layout of
/// config/conf.yaml: keys are peer RAFT addrs, values src/target pairs)
/// to every node's conf.yaml, restarting each member once so it picks
/// the map up (raft state below each dir survives; the restarted member
/// re-syncs via replication, per the process_failover respawn pattern).
/// Deviation from the per-node sketch: EVERY config carries the whole
/// map, because whichever node leads after the restart round is the one
/// whose `spawn_backup_map_init` seeds the keys.
async fn inject_backup_map(nodes: &mut [ProcNode], backups: &[String]) {
    let mut block = String::from("backup_target_map:\n");
    for i in 0..nodes.len() {
        let target = &backups[(i + 1) % backups.len()];
        block.push_str(&format!(
            "  \"{}\":\n    src: \"{}\"\n    target: \"{}\"\n",
            nodes[i].raft, nodes[i].resp, target
        ));
    }
    for node in nodes.iter_mut() {
        node.kill_now();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&node.config_path)
            .expect("open conf.yaml for append");
        f.write_all(block.as_bytes())
            .unwrap_or_else(|e| panic!("append backup_target_map: {e}"));
        node.respawn();
        wait_resp_ready(node, 60).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_target_map_moves_a_dead_nodes_band_to_a_live_backup() {
    let dir = tempfile::tempdir().expect("tempdir");

    // 3 nodes, each WITH a backup listener; node0 bootstraps and must
    // lead before the joiners connect through its raft-http addr.
    let (mut n0, b0) = spawn_node_backup(dir.path(), 0, true, None);
    wait_resp_ready(&mut n0, 90).await;
    let mut nodes = vec![n0];
    let mut backups = vec![b0];
    assert_eq!(
        wait_leader(&nodes, 120).await,
        0,
        "node0 must lead before joins\n{}",
        all_ctx(&nodes)
    );
    let join = nodes[0].http.clone();
    for id in 1..3 {
        let (mut n, b) = spawn_node_backup(dir.path(), id, false, Some(&join));
        wait_resp_ready(&mut n, 60).await;
        nodes.push(n);
        backups.push(b);
    }

    // Wire the cyclic map BEFORE cluster init: victim band i would move
    // to the backup listener of process (i+1)%3 -- a different process,
    // so the takeover target survives the victim's death.
    inject_backup_map(&mut nodes, &backups).await;
    let leader = wait_leader(&nodes, 90).await;

    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    let instances = binds.join(",");
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 90).await;

    // Victim: a NON-leader; its successor process holds the takeover.
    let victim = (leader + 1) % 3;
    let successor = (victim + 1) % 3;
    let backup_addr = backups[successor].clone();
    eprintln!("leader={leader} victim={victim} successor={successor} backup={backup_addr}");

    // The leader seeded the map: "src,target" bulk under the victim's
    // raft-addr key (spawn_backup_map_init, leader-only, 1s ticker).
    let map_key = format!("backup_target_map_{}", nodes[victim].raft);
    let map_val = format!("{},{}", nodes[victim].resp, backup_addr);
    poll_raft_get(&nodes[leader], &map_key, &map_val, 60).await;

    // A key owned by the victim's band, seeded on the victim itself
    // (retry through the post-init settle window).
    let key = band_key(&nodes[leader], victim).await;
    let slot = keyslot(&nodes[leader], &key).await;
    poll_reply(
        &nodes,
        victim,
        &[b"set", key.as_slice(), b"vfail"],
        60,
        |r| r == b"+OK",
    )
    .await;
    poll_reply(&nodes, victim, &[b"get", key.as_slice()], 30, |r| {
        r == b"$5\r\nvfail"
    })
    .await;

    // SIGKILL the victim. Within one 5s probe tick the leader marks the
    // Failed observation and swaps the victim's resp addr for the
    // successor's backup addr; after the 3s topology resync the survivor
    // MOVEDs the key to the EXACT backup address (in-place replacement
    // keeps the slot and band unchanged).
    nodes[victim].kill_now();
    let moved = format!("-MOVED {slot} {backup_addr}");
    poll_reply(&nodes, leader, &[b"get", key.as_slice()], 90, |r| {
        r == moved.as_bytes()
    })
    .await;

    // The takeover listener: reads serve from its OWN (separate, empty)
    // store -> RESP null; writes hit the read-only gate.
    poll_addr(
        &nodes[successor],
        &backup_addr,
        &[b"get", key.as_slice()],
        b"$-1",
        30,
    )
    .await;
    let ro = cmd_one_shot(&backup_addr, TOKEN, &[b"set", b"bk:wo", b"x"]).await;
    assert!(
        ro.starts_with(b"-READONLY") && contains_bytes(&ro, READONLY_ERR),
        "set on backup listener must reply -READONLY, got {ro:?}"
    );

    // Restart the corpse: the next successful probe is a Resumed
    // observation, the band swaps back to the victim's original resp
    // addr, and the replicated instances list is restored verbatim.
    nodes[victim].respawn();
    wait_resp_ready(&mut nodes[victim], 60).await;
    let back = format!("-MOVED {slot} {}", nodes[victim].resp);
    poll_reply(&nodes, leader, &[b"get", key.as_slice()], 90, |r| {
        r == back.as_bytes()
    })
    .await;
    poll_raft_get(
        &nodes[leader],
        "cluster_slots_stable_instances",
        &instances,
        60,
    )
    .await;
}
