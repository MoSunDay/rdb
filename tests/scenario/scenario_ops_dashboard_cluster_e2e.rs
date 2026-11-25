//! Multi-store scenario #131 -- an ops dashboard polls a REAL two-node
//! cluster: the CLUSTER panels (INFO / NODES / SLOTS / KEYSLOT) and the
//! RAFT panels (stats / leader / nodes) must line up with each other and
//! with the topology the fixture built (node ids = the md5 rule over the
//! resp bind, equal slot bands 0-8192 / 8193-16383 covering all 16384
//! slots, one term / leader / voter list on both nodes), a -MOVED probe
//! follows CLUSTER KEYSLOT to the owning node and back, `raft get
//! sql_nodes` lists both MySQL binds, and a dashboard snapshot row on
//! the leader's MySQL plane reads back exactly what RESP reported.
//! SURFACES: RESP (CLUSTER / RAFT / GET / SET) + MySQL. Two REAL nodes,
//! MySQL + sql_rpc on both.
//!
//! Deviation (pinned, not a bug): `CLUSTER INFO` spells the state
//! `cluster_state:true` (a Rust bool rendered with Display; Redis says
//! `ok`), src/command/cluster.rs:189-198; `CLUSTER test` always answers
//! the hard-coded `-MOVED 5465 127.0.0.1:32681` whatever the cluster
//! health, src/command/cluster.rs:283-285 (Go quirk kept on purpose), and
//! its match arm is the one case-sensitive arm of the dispatcher (cluster.rs:47
//! vs the dual-case `info`/`nodes`/`slots`/`keyslot` arms), so uppercase
//! `CLUSTER TEST` falls through to the help text (cluster.rs:51 -> :117).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, run, s, wait_table};
use common::{cluster_init, wait_cluster_nodes_list_all, wait_leader};
use md5::{Digest, Md5};
use scenario_common::{
    front_world, int_of, ok, r, rb, rt, spawn_front_node, wait_fronts, world_dir,
    FrontSpec,
};
use std::time::{Duration, Instant};

const TAG: &str = "ops131";
const SLOTS: u32 = 16384;
/// Equal split of the slot space over two nodes (the router's inclusive
/// `slot <= (index+1)*per` rule, recomputed here).
const PER_NODE: u32 = SLOTS / 2;

/// CRC-16/XMODEM (poly 0x1021, init 0) computed BITWISE -- independent of
/// the server's table-driven `hash::crc16`.
fn crc16(key: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in key {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Redis hash tag: the first `{...}` with non-empty content, else the key.
fn hash_tag(key: &[u8]) -> &[u8] {
    if let Some(start) = key.iter().position(|&b| b == b'{') {
        let rest = &key[start + 1..];
        if let Some(end) = rest.iter().position(|&b| b == b'}') {
            if end > 0 {
                return &rest[..end];
            }
        }
    }
    key
}

fn slot_of(key: &[u8]) -> u32 {
    u32::from(crc16(hash_tag(key)) % 16384)
}

/// Cluster node id: 32 hex chars of md5(bind) + the digest's last 8.
fn node_id(addr: &str) -> String {
    let d = hex::encode(Md5::digest(addr.as_bytes()));
    format!("{d}{}", &d[24..])
}

fn port_of(addr: &str) -> u16 {
    addr.rsplit(':').next().and_then(|p| p.parse().ok()).expect("host:port")
}

/// Band of node `k` of a two-node equal split, as CLUSTER NODES renders it.
fn band(k: usize) -> String {
    let start = if k == 0 { 0 } else { PER_NODE * k as u32 + 1 };
    let end = if k == 1 { SLOTS - 1 } else { PER_NODE * (k as u32 + 1) };
    format!("{start}-{end}")
}

/// Payload of the FIRST bulk of a one-shot reply (`$N\r\n` + N bytes; the
/// length bound keeps multi-line bodies such as CLUSTER NODES intact).
fn first_bulk(reply: &[u8]) -> String {
    let text = common::lite::text(reply);
    let (head, rest) = text.split_once("\r\n").unwrap_or((&text, ""));
    let n: usize = head
        .strip_prefix('$')
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("not a bulk reply: {text:?}"));
    String::from_utf8_lossy(&rest.as_bytes()[..n.min(rest.len())]).into_owned()
}

/// Wire-order cells of a full-drain reply: `:n` -> "n", `$len` -> the
/// payload that follows (null -> ""), array heads skipped.
fn cells(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if let Some(n) = t.strip_prefix(':') {
            out.push(n.to_string());
        } else if let Some(len) = t.strip_prefix('$') {
            if let Ok(n) = len.parse::<usize>() {
                out.push(if n == 0 { String::new() } else { toks.get(i + 1).unwrap_or(&"").to_string() });
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// `key: value` rows of a panel body (CLUSTER INFO lines / RAFT stats bulks).
fn fields<I: IntoIterator<Item = String>>(rows: I) -> Vec<(String, String)> {
    rows.into_iter()
        .filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect()
}

fn field<'a>(rows: &'a [(String, String)], key: &str) -> &'a str {
    rows.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("no {key}: {rows:?}"))
}

/// `(id, addr, port, myself?, slot ranges)` rows of a CLUSTER NODES body.
fn node_rows(body: &str) -> Vec<(String, String, u16, bool, String)> {
    body.split("\r\n").filter(|l| !l.is_empty()).map(|l| {
        let t: Vec<&str> = l.split_whitespace().collect();
        assert_eq!(t.len(), 9, "CLUSTER NODES line: {l:?}");
        let (addr, port) = t[1].split_once('@').unwrap();
        (t[0].to_string(), addr.to_string(), port.parse().unwrap(), t[2] == "myself,master", t[8].to_string())
    }).collect()
}

/// `(start, end, ip, port, id)` rows of a CLUSTER SLOTS reply (each row
/// is `*3 :start :end *3 $ip :port $id`; the array heads are skipped).
fn slot_rows(full: &str) -> Vec<(u32, u32, String, u16, String)> {
    let head = full.split("\r\n").next().unwrap_or("");
    let c = cells(full);
    assert_eq!(c.len() % 5, 0, "CLUSTER SLOTS rows of 5 cells: {full}");
    assert_eq!(head.strip_prefix('*').and_then(|n| n.parse::<usize>().ok()), Some(c.len() / 5), "array head {head:?}");
    c.chunks(5).map(|r| (r[0].parse().unwrap(), r[1].parse().unwrap(), r[2].clone(), r[3].parse().unwrap(), r[4].clone())).collect()
}

/// Poll `raft get sql_nodes` (JSON: raft addr -> binds) until both nodes
/// registered their resp/mysql binds with a non-empty sql_rpc port.
async fn wait_registry(leader: &str, nodes: &[common::ProcNode], secs: u64) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let reply = r(leader, &[b"raft", b"get", b"sql_nodes"]).await;
        if !reply.starts_with(b"$-1") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&first_bulk(&reply)) {
                let ready = nodes.iter().all(|n| {
                    let e = &v[n.raft.as_str()];
                    e["resp"] == n.resp && e["mysql"] == n.mysql && e["http"] == n.http && e["sql_rpc"] != ""
                });
                if ready {
                    return v;
                }
            }
        }
        assert!(Instant::now() < deadline, "sql_nodes registry never converged: {reply:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn cluster_raft_panels_line_up_with_probe_and_registry() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { mysql: true, sql_rpc: true, ..Default::default() };
    let (n0, _) = front_world(TAG, &spec).await;
    let join = n0.node.http.clone();
    let mut n1 = spawn_front_node(&world_dir("ops131b"), 1, false, Some(&join), &spec);
    wait_fronts(&mut n1, true, false, false).await;
    let nodes = [n0.node, n1.node];
    let leader = wait_leader(&nodes, 30).await;
    let peers = [nodes[0].resp.clone(), nodes[1].resp.clone()];
    cluster_init(&nodes[leader], &peers).await;
    wait_cluster_nodes_list_all(&nodes, &peers, 90).await;
    let follower = 1 - leader;
    let ids = [node_id(&peers[0]), node_id(&peers[1])];

    // a) The dashboard polls every panel on both nodes.
    let t = scenario_common::perf::now();
    let mut infos = Vec::new();
    let mut bodies = Vec::new();
    let mut slots = Vec::new();
    let mut stats = Vec::new();
    let mut stats_after = Vec::new();
    for n in &nodes {
        // RAFT stats right BEFORE and AFTER the panels: the ticker keeps
        // committing entries, so INFO's epoch (term ++ commit_index) is
        // bracketed by the two snapshots instead of compared to one.
        stats.push(fields(cells(&common::lite::text(&common::lite::cmd_full_reply_wait(&n.resp, common::TOKEN, &[b"RAFT", b"STATS"], 25, 10_000).await))));
        infos.push(fields(first_bulk(&r(&n.resp, &[b"CLUSTER", b"INFO"]).await).split('\n').map(str::to_string)));
        bodies.push(first_bulk(&r(&n.resp, &[b"CLUSTER", b"NODES"]).await));
        slots.push(common::lite::text(&common::lite::cmd_full_reply_wait(&n.resp, common::TOKEN, &[b"CLUSTER", b"SLOTS"], 25, 10_000).await));
        stats_after.push(fields(cells(&common::lite::text(&common::lite::cmd_full_reply_wait(&n.resp, common::TOKEN, &[b"RAFT", b"STATS"], 25, 10_000).await))));
    }
    let raft_nodes = [rt(&nodes[0].resp, &[b"RAFT", b"NODES"]).await, rt(&nodes[1].resp, &[b"RAFT", b"NODES"]).await];
    let raft_leader = [rt(&nodes[0].resp, &[b"RAFT", b"LEADER"]).await, rt(&nodes[1].resp, &[b"RAFT", b"LEADER"]).await];
    let cluster_test = rt(&nodes[leader].resp, &[b"CLUSTER", b"test"]).await;
    let cluster_test_upper = rt(&nodes[leader].resp, &[b"CLUSTER", b"TEST"]).await;
    let keyslot = int_of(&r(&nodes[leader].resp, &[b"CLUSTER", b"KEYSLOT", b"foo"]).await);
    scenario_common::perf::within("ops131/cluster-panels", t, Duration::from_secs(2));

    // b) RAFT panels: one term, one leader, both voters, on both nodes;
    //    CLUSTER INFO's epoch is this node's term+commit_index.
    assert_eq!(field(&stats[0], "term"), field(&stats[1], "term"), "one term");
    assert_eq!(raft_leader[0], format!("+raft addr: {}", nodes[leader].raft));
    assert_eq!(raft_leader[1], raft_leader[0], "both nodes resolve the same leader");
    for i in 0..2 {
        let st = &stats[i];
        let state = if i == leader { "Leader" } else { "Follower" };
        assert_eq!(field(st, "state"), state);
        assert_eq!(field(st, "num_peers"), "1");
        assert_eq!(field(st, "protocol_version"), "3");
        assert_eq!(field(st, "applied_index"), field(st, "commit_index"));
        assert!(field(st, "commit_index").parse::<u64>().unwrap() >= 1, "init entries applied: {st:?}");
        let voters = field(st, "latest_configuration");
        for addr in [&nodes[0].raft, &nodes[1].raft] {
            assert!(voters.contains(&format!("{{Suffrage:Voter ID:{addr} Address:{addr}}}")), "{voters}");
        }
        assert_eq!(voters.matches("Suffrage:Voter").count(), 2, "exactly two voters: {voters}");
        assert_eq!(raft_nodes[i], format!("+{} [{state}], nodes: {voters}", nodes[i].raft));
        assert_eq!(raft_nodes[i].contains("[Leader]"), i == leader, "matches wait_leader");
        // CLUSTER INFO on the same node.
        let info = &infos[i];
        assert_eq!(field(info, "cluster_state"), "true");
        for (k, v) in [("cluster_slots_assigned", "16384"), ("cluster_slots_ok", "16384"), ("cluster_slots_pfail", "0"), ("cluster_slots_fail", "0"), ("cluster_known_nodes", "2"), ("cluster_size", "2"), ("cluster_stats_messages_sent", "0"), ("cluster_stats_messages_received", "0")] {
            assert_eq!(field(info, k), v, "{k}");
        }
        let term = field(st, "term");
        let epoch = field(info, "cluster_current_epoch");
        let commit: u64 = epoch.strip_prefix(term).unwrap_or_else(|| panic!("epoch {epoch} is not term {term} ++ commit: {info:?}")).parse().unwrap_or_else(|_| panic!("epoch {epoch}: {info:?}"));
        let before: u64 = field(st, "commit_index").parse().unwrap();
        let after: u64 = field(&stats_after[i], "commit_index").parse().unwrap();
        assert_eq!(field(&stats_after[i], "term"), term, "no election between the two RAFT stats snapshots");
        assert!(before <= commit && commit <= after, "epoch {epoch} = term {term} ++ commit inside [{before}, {after}]");
        assert_eq!(field(info, "cluster_my_epoch"), epoch);
        // CLUSTER NODES: both ids, both binds, myself flag, equal bands.
        let rows = node_rows(&bodies[i]);
        assert_eq!(rows.len(), 2, "{}", bodies[i]);
        for k in 0..2 {
            let row = rows.iter().find(|r| r.1 == peers[k]).unwrap_or_else(|| panic!("{k} missing: {rows:?}"));
            assert_eq!(row.0, ids[k], "id is the md5 rule over the bind");
            assert_eq!(row.2, port_of(&peers[k]));
            assert_eq!(row.4, band(k));
        }
        let mine: Vec<_> = rows.iter().filter(|r| r.3).collect();
        assert_eq!((mine.len(), mine[0].1.as_str()), (1, peers[i].as_str()), "myself marks the polled node");
        // CLUSTER SLOTS: ranges tile 0..=16383 exactly, same ids/addrs.
        let sr = slot_rows(&slots[i]);
        assert_eq!(sr.len(), 2, "{}", slots[i]);
        let total: u32 = sr.iter().map(|r| r.1 - r.0 + 1).sum();
        assert_eq!(total, SLOTS, "every slot assigned once");
        assert_eq!((sr[0].0, sr[1].1), (0, SLOTS - 1), "tiles from 0 to 16383");
        assert_eq!(sr[0].1 + 1, sr[1].0, "contiguous");
        for k in 0..2 {
            assert_eq!(sr[k], (band_range(k).0, band_range(k).1, "127.0.0.1".to_string(), port_of(&peers[k]), ids[k].clone()));
        }
    }
    // c) The two quirks the dashboard must know about.
    assert_eq!(cluster_test_upper, "+cluster [ help | nodes | slots | test | keyslot | getkeysinslot | setslot ]", "CLUSTER TEST (uppercase) is the one case-sensitive arm -> help text");
    assert_eq!(cluster_test, "-MOVED 5465 127.0.0.1:32681", "CLUSTER test is a hard-coded reply");
    assert_eq!(keyslot, i64::from(crc16(b"foo") % 16384), "KEYSLOT of an untagged key");

    // d) Probe: a key whose tag lands in node1's band is -MOVED from
    //    node0 to node1's resp bind (slot == KEYSLOT), served there, and
    //    -MOVED again on node0; a home key mirrors it the other way.
    let mut home = String::new();
    let mut foreign = String::new();
    for i in 0..64u32 {
        let k = format!("{{{TAG}-k{i}}}");
        if slot_of(k.as_bytes()) <= PER_NODE && home.is_empty() {
            home = k.clone();
        }
        if slot_of(k.as_bytes()) > PER_NODE && foreign.is_empty() {
            foreign = k;
        }
        if !home.is_empty() && !foreign.is_empty() {
            break;
        }
    }
    assert!(!home.is_empty() && !foreign.is_empty(), "{home:?} {foreign:?}");
    let (fslot, hslot) = (slot_of(foreign.as_bytes()), slot_of(home.as_bytes()));
    assert_eq!(int_of(&r(&peers[0], &[b"CLUSTER", b"KEYSLOT", foreign.as_bytes()]).await), i64::from(fslot));
    let moved = rt(&peers[0], &[b"GET", foreign.as_bytes()]).await;
    assert_eq!(moved, format!("-MOVED {fslot} {}", peers[1]));
    ok(&r(&peers[1], &[b"SET", foreign.as_bytes(), b"laptop-x1"]).await, "set on the owner");
    assert_eq!(rb(&peers[1], &[b"GET", foreign.as_bytes()]).await, "laptop-x1");
    assert_eq!(rt(&peers[0], &[b"GET", foreign.as_bytes()]).await, moved, "still redirected");
    assert_eq!(rb(&peers[0], &[b"GET", home.as_bytes()]).await, "", "home key served locally");
    ok(&r(&peers[0], &[b"SET", home.as_bytes(), b"v"]).await, "set home");
    assert_eq!(rt(&peers[1], &[b"GET", home.as_bytes()]).await, format!("-MOVED {hslot} {}", peers[0]));

    // e) Registry: `raft get sql_nodes` lists both MySQL binds.
    let reg = wait_registry(&nodes[leader].resp, &nodes, 30).await;
    assert_eq!(reg.as_object().map(|o| o.len()), Some(2), "{reg}");
    for n in &nodes {
        let e = &reg[n.raft.as_str()];
        assert_eq!((e["resp"].as_str(), e["mysql"].as_str()), (Some(n.resp.as_str()), Some(n.mysql.as_str())));
    }

    // f) SQL snapshot row on the leader's MySQL plane mirrors RESP.
    let mut c = connect_root(&nodes[leader]).await;
    let mut fc = connect_root(&nodes[follower]).await;
    ddl(&mut c, "CREATE TABLE ops_snapshot (panel VARCHAR(16) PRIMARY KEY, cluster_state VARCHAR(16) NOT NULL, known_nodes INT NOT NULL, slots_assigned INT NOT NULL, moved_slot INT NOT NULL, moved_target VARCHAR(48) NOT NULL)").await;
    wait_table(&mut fc, "ops_snapshot", true).await;
    let state = field(&infos[leader], "cluster_state").to_string();
    run(&mut c, &format!("INSERT INTO ops_snapshot VALUES ('cluster', '{state}', 2, 16384, {fslot}, '{}')", peers[1])).await;
    let back = common::mysql::rows(&mut c, "SELECT panel, cluster_state, known_nodes, slots_assigned, moved_slot, moved_target FROM ops_snapshot WHERE panel = 'cluster'").await;
    assert_eq!(back, vec![vec![s("cluster"), s(&state), int(2), int(16384), int(i64::from(fslot)), s(&peers[1])]]);
    assert_eq!(common::mysql::ints(&mut c, "SELECT COUNT(*) FROM ops_snapshot WHERE known_nodes = 2 AND slots_assigned = 16384 AND moved_target <> ''").await, vec![1]);
    assert_eq!(common::mysql::one(&mut c, "SELECT moved_target FROM ops_snapshot").await, s(&peers[1]));
    scenario_common::perf::scenario("ops131", t0);
}

/// `(start, end)` of node `k`'s band.
fn band_range(k: usize) -> (u32, u32) {
    let b = band(k);
    let (a, z) = b.split_once('-').unwrap();
    (a.parse().unwrap(), z.parse().unwrap())
}
