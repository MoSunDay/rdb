//! Multi-store scenario #149 -- a slot dry-run before a real migration on a
//! REAL two-node cluster (node0 bootstraps, node1 joins, `cluster init` carves
//! the bands). Every `{dry149}` key shares ONE slot (CRC-16 recomputed BITWISE,
//! cross-checked with CLUSTER KEYSLOT); string/hash/zset/list are seeded on the
//! owner, which marks the slot MIGRATING to the peer: hits stay local, every
//! miss is the exact `-ASK <slot> <peer>` line, GETKEYSINSLOT spends a
//! PHYSICAL-row budget on the source and is empty on the destination, STABLE
//! closes the ASK path, SETSLOT NODE on the leader moves ownership (source ->
//! -MOVED, the new owner serves an EMPTY slot: no data moves), argument errors
//! are exact, and one SQL audit row per probe outcome reads back verbatim.
//! SURFACES: RESP (CLUSTER/GET/HGET/ZSCORE/LINDEX) + MySQL. Two REAL nodes, MySQL + sql_rpc on both.
//! Deviation (pinned): slot 11520 sits in node1's band, so the migration runs node1 -> node0; the roles are probed past -MOVED, never hard-coded (the brief said node0 -> node1).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, pairs, run, wait_table};
use common::{cluster_init, wait_cluster_nodes_list_all, wait_leader};
use md5::{Digest, Md5};
use scenario_common::{
    front_world, ok, r, rb, rt, spawn_front_node, wait_fronts, world_dir, FrontSpec,
};
use std::time::{Duration, Instant};

const TAG: &str = "dry149";
/// A well-formed but unregistered 40-hex node id.
const BOGUS: &str = "0123456789abcdef0123456789abcdef01234567";
/// The physical rows the seeding writes, as `(kind byte, key, ordinal)`:
/// a raw string is the bare key (kind 0x00 -> no kind/len prefix); a typed
/// record is `kind | len(u32) | key [| suffix]` (codec.rs `data_key` /
/// `elem_key`; hash_ds.rs / list_ds.rs / zset_ds.rs layouts). Kinds:
/// HASH_META 0x02, HASH_FLD 0x03, LIST_META 0x04, LIST_R 0x06 (RPUSH),
/// ZSET_META 0x09, ZSET_MEMBER 0x0A, ZSET_SCORE 0x0B; the ordinal only
/// orders element rows sharing a kind.
const ROWS: [(u8, &str, u8); 10] = [
    (0x02, "hash:party", 0),
    (0x03, "hash:party", 0),
    (0x03, "hash:party", 1),
    (0x04, "list:bar", 0),
    (0x06, "list:bar", 0),
    (0x09, "zset:queue", 0),
    (0x0A, "zset:queue", 0),
    (0x0B, "zset:queue", 0),
    (0x00, "str:host", 0),
    (0x00, "str:menu", 0),
];
/// The seeded user keys, sorted (hash + list + zset + two raw strings).
const SEED: [&str; 5] = ["hash:party", "list:bar", "str:host", "str:menu", "zset:queue"];

/// CRC-16/XMODEM (poly 0x1021, init 0) computed BITWISE -- independent of
/// the server's table-driven `hash::crc16`.
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
    key.iter().position(|&b| b == b'{').and_then(|s| {
        key[s + 1..].iter().position(|&b| b == b'}').filter(|e| *e > 0).map(|e| &key[s + 1..s + 1 + e])
    }).unwrap_or(key)
}

fn slot_of(key: &[u8]) -> u16 {
    crc16(hash_tag(key)) % 16384
}

/// Cluster node id: md5(bind) hex + its last 8 chars (`utils::md5_with40`).
fn node_id(addr: &str) -> String {
    let d = hex::encode(Md5::digest(addr.as_bytes()));
    format!("{d}{}", &d[24..])
}

fn key(name: &str) -> String {
    format!("{{{TAG}}}{name}")
}

/// GETKEYSINSLOT's `count` is a PHYSICAL-row budget (cluster.rs:77-115
/// collects `count` rows in RocksDB byte order, then keeps only rows
/// whose suffix is empty): the distinct user keys of the first `count`
/// rows of the ROWS model, sorted.
fn budgeted(count: usize) -> Vec<String> {
    let mut rows: Vec<(Vec<u8>, &str)> = ROWS
        .iter()
        .map(|&(kind, name, ord)| {
            let bytes = key(name).into_bytes();
            let mut row = Vec::new();
            if kind != 0x00 {
                row.push(kind);
                row.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            }
            row.extend_from_slice(&bytes);
            row.push(ord);
            (row, name)
        })
        .collect();
    rows.sort();
    let mut out: Vec<String> = Vec::new();
    for (_, name) in rows.into_iter().take(count) {
        let k = key(name);
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out.sort();
    out
}

fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Wire-order cells of a full-drain reply: `:n` -> "n", `$len` -> the
/// payload that follows (array heads skipped).
fn cells(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Some(n) = toks[i].strip_prefix(':') {
            out.push(n.to_string());
        } else if toks[i].strip_prefix('$').is_some_and(|h| h.parse::<usize>().is_ok()) {
            out.push(toks.get(i + 1).unwrap_or(&"").to_string());
            i += 1;
        }
        i += 1;
    }
    out
}

/// `CLUSTER GETKEYSINSLOT <slot> <count>` -> the user keys, sorted.
async fn keys_in_slot(addr: &str, slot: u16, count: usize) -> Vec<String> {
    let full = common::lite::text(
        &common::lite::cmd_full_reply_wait(
            addr,
            common::TOKEN,
            &argv(&["CLUSTER", "GETKEYSINSLOT", &slot.to_string(), &count.to_string()]),
            25,
            10_000,
        )
        .await,
    );
    assert!(full.starts_with('*'), "GETKEYSINSLOT reply: {full}");
    let mut keys = cells(&full);
    keys.sort();
    keys
}

/// Both nodes' resp binds registered in the raft `sql_nodes` map with a
/// non-empty sql_rpc port (the registry self-registers on a 3s ticker).
async fn wait_registry(leader: &str, binds: &[String], secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let reg = rt(leader, &[b"raft", b"get", b"sql_nodes"]).await;
        let ready = binds.iter().all(|b| reg.contains(b.as_str())) && !reg.contains("\"sql_rpc\":\"\"");
        if ready {
            return;
        }
        assert!(Instant::now() < deadline, "sql_nodes never converged: {reg}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn slot_dryrun_asks_then_moves_ownership() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { mysql: true, sql_rpc: true, ..Default::default() };
    let (n0, _) = front_world(TAG, &spec).await;
    let join = n0.node.http.clone();
    let mut n1 = spawn_front_node(&world_dir("dry149b"), 1, false, Some(&join), &spec);
    wait_fronts(&mut n1, true, false, false).await;
    let nodes = [n0.node, n1.node];
    let leader = wait_leader(&nodes, 30).await;
    let peers = [nodes[0].resp.clone(), nodes[1].resp.clone()];
    cluster_init(&nodes[leader], &peers).await;
    wait_cluster_nodes_list_all(&nodes, &peers, 90).await;
    let lead = nodes[leader].resp.clone();

    // a) The slot, recomputed bitwise, and its owner: exactly one node is
    //    not redirected; the other names it in a -MOVED to the same slot.
    let menu = key("str:menu");
    let slot = slot_of(menu.as_bytes());
    assert_eq!(slot, 11520, "CRC16(dry149) % 16384, the band-1 slot the header pins");
    assert_eq!(rt(&lead, &[b"CLUSTER", b"KEYSLOT", menu.as_bytes()]).await, format!(":{slot}"), "CLUSTER KEYSLOT agrees with the bitwise CRC-16");
    assert_eq!(rt(&lead, &[b"CLUSTER", b"KEYSLOT", key("str:host").as_bytes()]).await, format!(":{slot}"), "every key of the tag shares the slot");
    let probes = [
        rt(&peers[0], &[b"GET", menu.as_bytes()]).await,
        rt(&peers[1], &[b"GET", menu.as_bytes()]).await,
    ];
    let src_i = if probes[0] == "$-1" { 0 } else { 1 };
    assert_eq!(probes[src_i], "$-1", "the owner holds nothing yet: {probes:?}");
    let (src, dst) = (peers[src_i].clone(), peers[1 - src_i].clone());
    assert_eq!(probes[1 - src_i], format!("-MOVED {slot} {src}"), "the other node redirects to the owner");
    let (src_id, dst_id) = (node_id(&src), node_id(&dst));

    // b) Mixed families seeded on the owner (all in the one slot).
    ok(&r(&src, &[b"SET", menu.as_bytes(), b"omakase"]).await, "seed menu");
    ok(&r(&src, &[b"SET", key("str:host").as_bytes(), b"desk-7"]).await, "seed host");
    assert_eq!(rt(&src, &[b"HSET", key("hash:party").as_bytes(), b"name", b"Duval", b"phone", b"555-0142"]).await, ":2", "hset");
    assert_eq!(rt(&src, &[b"ZADD", key("zset:queue").as_bytes(), b"3", b"patron-a"]).await, ":1", "zadd");
    assert_eq!(rt(&src, &[b"RPUSH", key("list:bar").as_bytes(), b"stool-1"]).await, ":1", "rpush");
    let mut seeded: Vec<String> = SEED.iter().map(|k| key(k)).collect();
    seeded.sort();
    // c) The dry run opens: the owner marks the slot MIGRATING to the peer.
    ok(&r(&src, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), "MIGRATING", &dst_id])).await, "setslot migrating");

    // d) Hot block -- the probe inventory while MIGRATING: hits (every
    //    family) are served locally, misses are the exact -ASK line, a
    //    missing FIELD of a present key is a plain nil, and GETKEYSINSLOT
    //    is a physical-row budget on the source / empty on the destination.
    let t = scenario_common::perf::now();
    let ask = format!("-ASK {slot} {dst}");
    let mut audit: Vec<(i64, String)> = Vec::new();
    audit.push((1, rb(&src, &[b"GET", menu.as_bytes()]).await));
    audit.push((2, rb(&src, &[b"HGET", key("hash:party").as_bytes(), b"name"]).await));
    audit.push((3, rb(&src, &[b"ZSCORE", key("zset:queue").as_bytes(), b"patron-a"]).await));
    audit.push((4, rb(&src, &[b"LINDEX", key("list:bar").as_bytes(), b"0"]).await));
    let hits: Vec<String> = audit.iter().map(|(_, v)| v.clone()).collect();
    let field_miss = rt(&src, &[b"HGET", key("hash:party").as_bytes(), b"nosuchfield"]).await;
    audit.push((5, rt(&src, &[b"GET", key("str:nobody").as_bytes()]).await));
    audit.push((6, rt(&src, &[b"ZSCORE", key("zset:ghost").as_bytes(), b"patron-a"]).await));
    let full = keys_in_slot(&src, slot, ROWS.len()).await;
    let wide = keys_in_slot(&src, slot, 1000).await;
    let short = keys_in_slot(&src, slot, ROWS.len() - 1).await;
    let tight = keys_in_slot(&src, slot, 3).await;
    let none = keys_in_slot(&dst, slot, 1000).await;
    scenario_common::perf::within("dry149/ask-probe-block", t, Duration::from_secs(2));
    assert_eq!(hits, ["omakase", "Duval", "3", "stool-1"], "string/hash/zset/list hits are served locally while MIGRATING");
    assert_eq!(field_miss, "$-1", "a missing FIELD of a present key is nil, not -ASK");
    assert_eq!((audit[4].1.as_str(), audit[5].1.as_str()), (ask.as_str(), ask.as_str()), "every miss answers the exact -ASK line to the destination");
    assert_eq!(full, seeded, "count 10 = the physical footprint: exactly the seeded keys");
    assert_eq!(wide, seeded, "a larger count still returns only the existing keys");
    // Typed kind bytes (0x02..0x0B) sort ahead of the raw strings (`{` =
    // 0x7B): eight typed rows, then str:host, then str:menu -- so three
    // rows are ONE key (hash meta + its two fields), nine rows drop the
    // LAST string, and ten rows are exactly the seeded set.
    assert_eq!(budgeted(ROWS.len()), seeded, "the model's full budget is the seeded set");
    assert_eq!(full, seeded, "count 10 = the physical footprint: exactly the seeded keys");
    assert_eq!(wide, seeded, "a larger count still returns only the existing keys");
    assert_eq!(short, budgeted(ROWS.len() - 1), "one row short = the model's first nine rows: {short:?}");
    assert!(!short.contains(&menu), "the dropped key is the last string in byte order");
    assert_eq!(short.len(), seeded.len() - 1, "one physical row short drops exactly one key");
    assert_eq!(tight, [key("hash:party")], "three rows are spent on the hash alone: {tight:?}");
    assert!(full.iter().all(|k| slot_of(k.as_bytes()) == slot), "every listed key hashes to the slot");
    assert_eq!(none, Vec::<String>::new(), "the destination holds none of the slot");

    // e) The real error strings of SETSLOT; none of them touches the gate.
    for sub in ["MIGRATING", "IMPORTING", "NODE"] {
        assert_eq!(rt(&src, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), sub, BOGUS])).await, format!("-ERR Unknown node {BOGUS}"), "{sub} with an unknown node id");
    }
    assert_eq!(
        rt(&src, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), "FROZEN"])).await,
        "-ERR Unknown subcommand or wrong number of arguments for 'cluster|setslot'.                  Try CLUSTER SETSLOT <slot> (MIGRATING|IMPORTING|STABLE|NODE <node-id>)."
    );
    assert_eq!(rt(&src, &argv(&["CLUSTER", "SETSLOT", &slot.to_string()])).await, "-ERR wrong number of arguments for 'cluster|setslot' command");
    for bad in ["99999", "abc", "-1"] {
        assert_eq!(rt(&src, &argv(&["CLUSTER", "SETSLOT", bad, "MIGRATING", &dst_id])).await, "-ERR Invalid slot", "slot {bad}");
    }
    assert_eq!(rt(&src, &[b"GET", key("str:nobody").as_bytes()]).await, ask, "the failed calls left the slot MIGRATING");
    // f) The destination's IMPORTING gate serves only ASKING connections.
    ok(&r(&dst, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), "IMPORTING", &src_id])).await, "setslot importing on the destination");
    assert_eq!(rt(&dst, &[b"GET", menu.as_bytes()]).await, format!("-MOVED {slot} {src}"), "a plain read on the importing node goes back to the source");
    // g) STABLE closes the ASK path on the source; ownership is untouched.
    ok(&r(&src, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), "STABLE"])).await, "setslot stable on the source");
    audit.push((7, rt(&src, &[b"GET", key("str:nobody").as_bytes()]).await));
    assert_eq!(audit[6].1, "$-1", "after STABLE a miss is a plain nil");
    assert_eq!(rb(&src, &[b"GET", menu.as_bytes()]).await, "omakase", "STABLE keeps the owner serving");
    ok(&r(&dst, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), "STABLE"])).await, "setslot stable on the destination");
    assert_eq!(rt(&dst, &[b"GET", menu.as_bytes()]).await, format!("-MOVED {slot} {src}"), "ownership routing again");
    // h) Ownership moves on the LEADER (a follower's NODE is local-only and
    //    the 3s topology sync re-reads the raft key); the source redirects
    //    to the new owner, the new owner serves an EMPTY slot.
    ok(&r(&lead, &argv(&["CLUSTER", "SETSLOT", &slot.to_string(), "NODE", &dst_id])).await, "setslot node on the leader");
    scenario_common::until_contains(&src, &[b"GET", menu.as_bytes()], "-MOVED", "source redirects", 15).await;
    audit.push((8, rt(&src, &[b"GET", menu.as_bytes()]).await));
    assert_eq!(audit[7].1, format!("-MOVED {slot} {dst}"), "the source now redirects to the destination");
    let deadline = Instant::now() + Duration::from_secs(15);
    let served = loop {
        let reply = rt(&dst, &[b"GET", menu.as_bytes()]).await;
        if !reply.starts_with("-MOVED") {
            break reply;
        }
        assert!(Instant::now() < deadline, "destination never took the slot: {reply}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    audit.push((9, served.clone()));
    assert_eq!(served, "$-1", "the new owner serves the slot locally -- EMPTY: the dry run moved no data");
    assert_eq!(keys_in_slot(&src, slot, 1000).await, seeded, "the data stayed on the source");
    assert_eq!(keys_in_slot(&dst, slot, 1000).await, Vec::<String>::new(), "nothing landed on the destination");

    // i) One SQL audit row per probe outcome on the leader's MySQL plane
    //    (tables are slot-banded: the SELECT scatters over both bands).
    wait_registry(&lead, &peers, 30).await;
    let mut c = connect_root(&nodes[leader]).await;
    let mut follower = connect_root(&nodes[1 - leader]).await;
    ddl(&mut c, "CREATE TABLE slot_audit (id INT PRIMARY KEY, outcome VARCHAR(64) NOT NULL)").await;
    wait_table(&mut follower, "slot_audit", true).await;
    for (id, outcome) in &audit {
        run(&mut c, &format!("INSERT INTO slot_audit VALUES ({id}, '{outcome}')")).await;
    }
    let back = pairs(&mut c, "SELECT id, outcome FROM slot_audit ORDER BY id").await;
    assert_eq!(back, audit, "the audit rows are the RESP outcomes");
    assert_eq!(ints(&mut c, &format!("SELECT COUNT(*) FROM slot_audit WHERE outcome = '{ask}'")).await, vec![2], "two -ASK probes");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM slot_audit WHERE outcome = '$-1'").await, vec![2], "stable miss + empty new owner");
    assert_eq!(ints(&mut c, &format!("SELECT COUNT(*) FROM slot_audit WHERE outcome = '-MOVED {slot} {dst}'")).await, vec![1], "one ownership redirect");
    scenario_common::perf::scenario("dry149", t0);
}
