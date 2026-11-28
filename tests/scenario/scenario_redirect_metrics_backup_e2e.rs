//! Multi-store scenario #175 -- a SRE's redirect accounting drill on a two-node cluster with a
//! read-only backup listener: the shared histogram books every -MOVED/-ASK as `ack="true"` and
//! local executions as `ack="false"`, per process `type="normal"|"backup"` -- a SET + GET fired at
//! the non-owner land as ack=true set/get on node0 and the owner's local SET as ack=false set on
//! node1; an ASK for a missing key under `SETSLOT MIGRATING` counts as ack=true get; the backup
//! bind (own RESP addr, not in the topology) redirects EVERY keyed read with -MOVED to the band
//! owner -- before INIT it serves locally -- while SET and CLUSTER INIT on it answer READONLY.
//! SURFACES: monitor HTTP + RESP + backup listener. Two REAL nodes, fronts default.
//! Deviation (pinned): the backup listener shares the topology but not the RESP bind, so post-INIT
//! it answers -MOVED to the owner for every keyed read, even slots node0 owns (src/main.rs:491-497,
//! src/router.rs:30-48); a MIGRATING source serves a PRESENT key locally and only redirects missing
//! keys with -ASK (src/command/mod.rs:366-371); ack=true is observed for MOVED/ASK only (mod.rs:468-472).

use crate::common;
use crate::scenario_common;
use common::{cluster_init, wait_cluster_nodes_list_all, wait_leader};
use md5::{Digest, Md5};
use scenario_common::{
    front_world, http, ok, r, rb, rt, spawn_front_node, wait_fronts, world_dir, FrontSpec,
};
use std::time::{Duration, Instant};

const TAG: &str = "rmetrics175";
const READONLY: &str = "-READONLY You can't write against a read only replica.";
const LEADER: &str = "raft_stats{status=\"Leader\"}";
/// Go verbatim (typo included): every CLUSTER subcommand but INIT before INIT.
const NOT_READY: &str = "-cluster not ready, initialize the cluster with this command (cluster init [instanes01,instanes02,instance03])";
/// Two nodes: `16384 / 2` slots per band; the inclusive edge is slot 8192.
const EDGE: u16 = 8192;

// ---- model (pure) ------------------------------------------------------

/// CRC-16/XMODEM (Redis cluster slot polynomial).
fn crc16(key: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in key {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// Redis hash tag: the first non-empty `{...}`, else the whole key.
fn hash_tag(key: &[u8]) -> &[u8] {
    key.iter()
        .position(|&b| b == b'{')
        .and_then(|s| {
            key[s + 1..]
                .iter()
                .position(|&b| b == b'}')
                .filter(|e| *e > 0)
                .map(|e| &key[s + 1..s + 1 + e])
        })
        .unwrap_or(key)
}

fn slot_of(key: &[u8]) -> u16 {
    crc16(hash_tag(key)) % 16384
}

/// Band owner of `slot` over `n` stable nodes (src/router.rs `route`): the
/// first index with `slot <= (index+1)*per_node_slots`, the LAST absorbs the rest.
fn owner_of(slot: u16, n: usize) -> usize {
    let per = 16384 / n;
    (0..n)
        .find(|&i| i == n - 1 || usize::from(slot) <= (i + 1) * per)
        .unwrap_or(n - 1)
}

/// Cluster node id: md5(bind) hex + its last 8 chars (`utils::md5_with40`).
fn node_id(addr: &str) -> String {
    let d = hex::encode(Md5::digest(addr.as_bytes()));
    format!("{d}{}", &d[24..])
}

/// One reserved loopback port (process-wide reservation lives in `common`).
fn free_port() -> String {
    common::free_addr()
}

/// First `{rm175t<i>}` tag whose slot satisfies `pred`.
fn tag_where(pred: impl Fn(u16) -> bool) -> String {
    (0..400_000u32)
        .map(|i| format!("rm175t{i}"))
        .find(|tag| pred(crc16(tag.as_bytes()) % 16384))
        .expect("no rm175t<i> tag matched the slot predicate within 400000")
}

fn key_of(tag: &str, suffix: &str) -> String {
    format!("{{{tag}}}{suffix}")
}

/// Histogram count series as the exposition spells it (label names sorted).
fn series(ack: &str, mode: &str, typ: &str) -> String {
    format!("rdb_command_latency_count{{ack=\"{ack}\",mode=\"{mode}\",type=\"{typ}\"}}")
}

// ---- metrics shims -----------------------------------------------------

/// Value of one exposition sample line (`name{labels} value`); `None`
/// when the series is absent (prometheus omits a child never observed).
fn sample(body: &str, series: &str) -> Option<f64> {
    body.lines()
        .find(|l| l.starts_with(&format!("{series} ")))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

/// Movement of one series across two scrapes (an absent series counts 0).
fn moved(before: &str, after: &str, series: &str) -> f64 {
    sample(after, series).unwrap_or_default() - sample(before, series).unwrap_or_default()
}

/// One `/metrics` scrape: the exposition body.
async fn scrape(monitor: &str) -> String {
    let (status, body) = http(monitor, "GET", "/metrics", None).await;
    assert_eq!(status, 200, "/metrics on {monitor}: {body}");
    body
}

/// Poll until `raft_stats{status="Leader"}` reads 1 (the 5s ticker,
/// src/main.rs:178-186); returns that exposition body.
async fn raft_leader(monitor: &str, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let body = scrape(monitor).await;
        if sample(&body, LEADER) == Some(1.0) {
            return body;
        }
        assert!(Instant::now() < deadline, "raft gauge never reached Leader on {monitor}: {body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Poll PING until the listener answers `+PONG` (<= `secs`).
async fn pingable(addr: &str, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if rt(addr, &[b"PING"]).await == "+PONG" {
            return;
        }
        assert!(Instant::now() < deadline, "no PONG from {addr} within {secs}s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- scenario ----------------------------------------------------------

#[tokio::test]
async fn redirect_metrics_backup() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    assert_eq!(crc16(b"key") % 16384, 12539, "model: Redis slot of 'key'");
    assert_eq!(slot_of(b"{user1000}.following"), slot_of(b"user1000"), "model: hash tag");

    // a) Node0 carries the read-only backup listener (own RESP bind + store,
    //    shared topology + collector, src/main.rs:491-560); node1 joins it.
    let node0_dir = world_dir(TAG).join("node0");
    let backup = free_port();
    let spec0 = FrontSpec {
        extra_yaml: format!(
            "backup_bind: \"{backup}\"\nbackup_store_path: \"{}/backup_store/\"\n",
            node0_dir.display()
        ),
        ..Default::default()
    };
    let (f0, a0) = front_world(TAG, &spec0).await;
    let join = f0.node.http.clone();
    let mut f1 = spawn_front_node(&world_dir("rmetrics175b"), 1, false, Some(&join), &FrontSpec::default());
    wait_fronts(&mut f1, false, false, false).await;
    let nodes = [f0.node, f1.node];
    let (a1, monitor_a, monitor_b) = (nodes[1].resp.clone(), nodes[0].monitor.clone(), nodes[1].monitor.clone());
    assert_ne!(backup, a0, "the backup bind is its own RESP addr");
    pingable(&backup, 10).await;

    // b) Key placement by the band rule: k0 (+ a missing sibling) in node0's
    //    band (slot <= 8192), k1 in node1's. Before INIT every CLUSTER
    //    subcommand but INIT is gated with the Go "cluster not ready" line
    //    (src/command/cluster.rs:24-35), so KEYSLOT is checked after INIT.
    let (tag_a, tag_b) = (tag_where(|s| owner_of(s, 2) == 0), tag_where(|s| owner_of(s, 2) == 1));
    let (k0, miss, k1) = (key_of(&tag_a, "k"), key_of(&tag_a, "miss"), key_of(&tag_b, "k"));
    let (s0, s1) = (slot_of(k0.as_bytes()), slot_of(k1.as_bytes()));
    assert!(s0 <= EDGE && s1 > EDGE, "band placement: {s0} {s1}");
    assert_eq!(rt(&a0, &[b"CLUSTER", b"KEYSLOT", k0.as_bytes()]).await, NOT_READY, "pre-INIT KEYSLOT gate");

    // c) Pre-INIT bracket on the backup listener: empty topology -> a keyed
    //    read is LOCAL (nil); SET and CLUSTER INIT are READONLY; every one of
    //    them is observed on the shared collector as type="backup", ack=false.
    let b0 = scrape(&monitor_a).await;
    assert_eq!(rt(&backup, &[b"GET", k1.as_bytes()]).await, "$-1", "backup GET before INIT is local");
    assert_eq!(rt(&backup, &[b"SET", k1.as_bytes(), b"v"]).await, READONLY, "backup SET");
    assert_eq!(rt(&backup, &[b"CLUSTER", b"INIT", a0.as_bytes()]).await, READONLY, "backup CLUSTER INIT");
    let b1 = scrape(&monitor_a).await;
    assert_eq!(moved(&b0, &b1, &series("false", "get", "backup")), 1.0, "{b1}");
    assert_eq!(moved(&b0, &b1, &series("false", "set", "backup")), 1.0, "{b1}");
    assert_eq!(moved(&b0, &b1, &series("false", "cluster", "backup")), 1.0, "{b1}");
    assert_eq!(moved(&b0, &b1, &series("true", "get", "backup")), 0.0, "{b1}");

    // d) INIT: leader, CLUSTER INIT in node order, topology convergence; now
    //    CLUSTER KEYSLOT on the leader agrees with the bitwise slots.
    let leader = wait_leader(&nodes, 30).await;
    let peers = [a0.clone(), a1.clone()];
    cluster_init(&nodes[leader], &peers).await;
    wait_cluster_nodes_list_all(&nodes, &peers, 10).await;
    let lead = nodes[leader].resp.clone();
    assert_eq!(rt(&lead, &[b"CLUSTER", b"KEYSLOT", k0.as_bytes()]).await, format!(":{s0}"));
    assert_eq!(rt(&lead, &[b"CLUSTER", b"KEYSLOT", k1.as_bytes()]).await, format!(":{s1}"));

    // e) Hot block -- redirect accounting: scrapes of BOTH monitors bracket a
    //    SET + a GET fired at node0 for node1's key (-MOVED twice -> ack=true
    //    set/get on node0, nothing ack=false) and the owner's local SET (ack=false set on node1).
    let t = scenario_common::perf::now();
    let (ma0, mb0) = (scrape(&monitor_a).await, scrape(&monitor_b).await);
    let moved_line = format!("-MOVED {s1} {a1}");
    assert_eq!(rt(&a0, &[b"SET", k1.as_bytes(), b"v"]).await, moved_line, "SET at the non-owner");
    assert_eq!(rt(&a0, &[b"GET", k1.as_bytes()]).await, moved_line, "GET at the non-owner");
    ok(&r(&a1, &[b"SET", k1.as_bytes(), b"v"]).await, "owner SET k1");
    let (ma1, mb1) = (scrape(&monitor_a).await, scrape(&monitor_b).await);
    scenario_common::perf::within("redirmetrics/redirect-accounting", t, Duration::from_secs(2));
    assert_eq!(moved(&ma0, &ma1, &series("true", "set", "normal")), 1.0, "{ma1}");
    assert_eq!(moved(&ma0, &ma1, &series("true", "get", "normal")), 1.0, "{ma1}");
    assert_eq!(moved(&ma0, &ma1, &series("false", "set", "normal")), 0.0, "{ma1}");
    assert_eq!(moved(&ma0, &ma1, &series("false", "get", "normal")), 0.0, "{ma1}");
    assert_eq!(moved(&mb0, &mb1, &series("false", "set", "normal")), 1.0, "{mb1}");
    assert_eq!(moved(&mb0, &mb1, &series("true", "set", "normal")), 0.0, "{mb1}");
    assert_eq!(rb(&a1, &[b"GET", k1.as_bytes()]).await, "v", "the owner really holds k1");

    // f) ASK accounting: node0 owns slot s0 and marks it MIGRATING to node1;
    //    a PRESENT key is served locally (ack=false get), a MISSING key gets
    //    the exact -ASK line (ack=true get); SETSLOT STABLE closes the path.
    ok(&r(&a0, &[b"SET", k0.as_bytes(), b"home"]).await, "seed k0 on node0");
    let (slot_s, id_b) = (s0.to_string(), node_id(&a1));
    ok(
        &r(&a0, &[b"CLUSTER", b"SETSLOT", slot_s.as_bytes(), b"MIGRATING", id_b.as_bytes()]).await,
        "setslot migrating",
    );
    let (ea0, eb0) = (scrape(&monitor_a).await, scrape(&monitor_b).await);
    assert_eq!(rb(&a0, &[b"GET", k0.as_bytes()]).await, "home", "present key under MIGRATING: local");
    assert_eq!(rt(&a0, &[b"GET", miss.as_bytes()]).await, format!("-ASK {s0} {a1}"), "missing key under MIGRATING");
    let (ea1, eb1) = (scrape(&monitor_a).await, scrape(&monitor_b).await);
    assert_eq!(moved(&ea0, &ea1, &series("true", "get", "normal")), 1.0, "{ea1}");
    assert_eq!(moved(&ea0, &ea1, &series("false", "get", "normal")), 1.0, "{ea1}");
    assert_eq!(moved(&eb0, &eb1, &series("true", "get", "normal")), 0.0, "{eb1}");
    ok(&r(&a0, &[b"CLUSTER", b"SETSLOT", slot_s.as_bytes(), b"STABLE"]).await, "setslot stable");
    assert_eq!(rt(&a0, &[b"GET", miss.as_bytes()]).await, "$-1", "STABLE: plain nil again");

    // g) Post-INIT backup bracket: the backup bind is not a stable addr, so BOTH
    //    keys redirect with -MOVED to their band owner (even k0, which node0
    //    owns: the pinned deviation); writes and CLUSTER NODES stay READONLY
    //    (ack=false) while the redirects are booked as ack=true, type="backup".
    let (c0, d0) = (scrape(&monitor_a).await, scrape(&monitor_b).await);
    assert_eq!(rt(&backup, &[b"GET", k1.as_bytes()]).await, format!("-MOVED {s1} {a1}"), "backup GET k1");
    assert_eq!(rt(&backup, &[b"GET", k0.as_bytes()]).await, format!("-MOVED {s0} {a0}"), "backup GET k0");
    assert_eq!(rt(&backup, &[b"SET", k0.as_bytes(), b"v"]).await, READONLY, "backup SET after INIT");
    assert_eq!(rt(&backup, &[b"CLUSTER", b"NODES"]).await, READONLY, "backup CLUSTER NODES");
    assert_eq!(rt(&backup, &[b"PING"]).await, "+PONG", "backup PING passes the gate");
    let (c1, d1) = (scrape(&monitor_a).await, scrape(&monitor_b).await);
    assert_eq!(moved(&c0, &c1, &series("true", "get", "backup")), 2.0, "{c1}");
    assert_eq!(moved(&c0, &c1, &series("false", "get", "backup")), 0.0, "{c1}");
    assert_eq!(moved(&c0, &c1, &series("false", "set", "backup")), 1.0, "{c1}");
    assert_eq!(moved(&c0, &c1, &series("false", "cluster", "backup")), 1.0, "{c1}");
    assert_eq!(sample(&c1, &series("true", "get", "backup")), Some(2.0), "cumulative: the two redirects");
    assert_eq!(moved(&d0, &d1, &series("true", "get", "backup")), 0.0, "no backup series on node1");
    assert_eq!(sample(&d1, &series("true", "get", "backup")), None, "backup series live on node0 only: {d1}");

    // h) The 5s raft ticker publishes Leader on the leader's own monitor.
    let mons = [monitor_a.as_str(), monitor_b.as_str()];
    let expose = raft_leader(mons[leader], 10).await;
    assert_eq!(sample(&expose, LEADER), Some(1.0));
    scenario_common::perf::scenario("redirmetrics", t0);
}
