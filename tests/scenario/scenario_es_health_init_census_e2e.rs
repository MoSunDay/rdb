//! Multi-store scenario #174 -- a two-node rollout: before `CLUSTER INIT` every node answers
//! `_cluster/health` YELLOW `number_of_nodes: 1` with an EMPTY `_cat/indices` (header row only),
//! after INIT both GREEN `number_of_nodes: 2`. The census is per node: on the slot owner
//! `_cat/indices` lists each index with its `md5_with40` uuid and live `docs.count` (create adds,
//! update keeps, delete decrements), a foreign index is refused with 400 `routing_exception`
//! naming slot + owner, so A's census omits B's index; indexes straddle the inclusive band edge
//! (8192 -> node0, 8193 -> node1) and `_cat` as an index name falls to `/{index}` forms (owner 404, else 400).
//! SURFACES: ES HTTP + RESP (raft CLUSTER INIT / CLUSTER KEYSLOT). Two REAL nodes, fronts es.
//! Deviation (pinned): `GET /_cat/health` and `GET /_cat/indices/<pattern>` are not `_cat` routes;
//! they fall to the `/{index}` forms (src/es/router.rs:75-94, 216-222): 404 `no_handler_found_exception`
//! on the slot owner, 400 `routing_exception` elsewhere; `?v=true` is never read; `cat_indices` scans
//! the LOCAL store only (src/es/misc.rs:76-102) and ES `slot_check` has no hash tag (src/es/write.rs:76-102).

use crate::common;
use crate::scenario_common;
use common::{cluster_init, wait_cluster_nodes_list_all, wait_leader};
use md5::{Digest, Md5};
use scenario_common::{
    front_world, http, int_of, json_of, r, spawn_front_node, wait_fronts, world_dir, FrontSpec,
};
use std::time::{Duration, Instant};

const TAG: &str = "eshealth174";
/// Health bodies byte for byte (serde_json keeps the `json!` key order).
const YELLOW: &str = "{\"cluster_name\":\"rdb\",\"status\":\"yellow\",\"timed_out\":false,\
                      \"number_of_nodes\":1,\"active_primary_shards\":0}";
const GREEN: &str = "{\"cluster_name\":\"rdb\",\"status\":\"green\",\"timed_out\":false,\
                     \"number_of_nodes\":2,\"active_primary_shards\":0}";
const CAT_HEAD: &str = "health status index uuid pri rep docs.count store.size\n";
const MAPPINGS: &str = "{\"mappings\":{\"properties\":{\"sku\":{\"type\":\"keyword\"},\
                        \"qty\":{\"type\":\"long\"}}}}";
const DOC_A: &str = "{\"sku\":\"SKU-174-A\",\"qty\":3}";
const DOC_B: &str = "{\"sku\":\"SKU-174-B\",\"qty\":5}";
const DOC_C: &str = "{\"sku\":\"SKU-174-A\",\"qty\":9}";
const MATCH_ALL: &str = "{\"query\":{\"match_all\":{}}}";
const ROUTING: &str = "routing_exception";
const HINT: &str = "ES fan-out clients must target the owning node (RESP MOVED equivalent)";
const NO_HANDLER: &str = "no_handler_found_exception";
/// Two nodes: `16384 / 2` slots per band; the inclusive edge is slot 8192.
const EDGE: u16 = 8192;

// ---- model (pure) ------------------------------------------------------

/// CRC-16/XMODEM over the whole name (the ES slot has no hash tag).
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

fn slot_of(name: &str) -> u16 {
    crc16(name.as_bytes()) % 16384
}

/// Band owner of `slot` over `n` stable nodes (src/router.rs `route`): the
/// first index with `slot <= (index+1)*per_node_slots`, the LAST absorbs the rest.
fn owner_of(slot: u16, n: usize) -> usize {
    let per = 16384 / n;
    (0..n)
        .find(|&i| i == n - 1 || usize::from(slot) <= (i + 1) * per)
        .unwrap_or(n - 1)
}

/// Index uuid: md5 hex + its last 8 chars (`utils::md5_with40`).
fn uuid_of(name: &str) -> String {
    let d = hex::encode(Md5::digest(name.as_bytes()));
    format!("{d}{}", &d[24..])
}

/// The `n`-th (0-based) `idx174-{i}` whose slot satisfies `pred`.
fn pick(pred: impl Fn(u16) -> bool, n: usize) -> String {
    let mut hits = 0usize;
    for i in 0..400_000u32 {
        let name = format!("idx174-{i}");
        if pred(slot_of(&name)) {
            if hits == n {
                return name;
            }
            hits += 1;
        }
    }
    panic!("no idx174-<i> name matched the slot predicate within 400000");
}

/// The exact `_cat/indices` text for `(index, docs)` rows: the header, then
/// one `green open` line per index sorted by name (ES lists by name).
fn census(rows: &[(&str, u64)]) -> String {
    let mut rows: Vec<&(&str, u64)> = rows.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    let mut out = String::from(CAT_HEAD);
    for (name, docs) in rows {
        out.push_str(&format!("green open {name} {} 1 0 {docs} 0b\n", uuid_of(name)));
    }
    out
}

/// The 8.x error envelope: the HTTP status repeats inside and `root_cause`
/// mirrors type + reason.
fn error_is(reply: &str, status: u16, kind: &str, reason: &str) {
    let v = json_of(reply);
    assert_eq!(v["status"], status, "envelope status: {reply}");
    assert_eq!(v["error"]["type"], kind, "envelope type: {reply}");
    assert_eq!(v["error"]["reason"], reason, "envelope reason: {reply}");
    assert_eq!(v["error"]["root_cause"][0]["type"], kind, "root_cause type: {reply}");
    assert_eq!(v["error"]["root_cause"][0]["reason"], reason, "root_cause reason: {reply}");
}

// ---- ES shims ----------------------------------------------------------

/// `GET target` on an ES front: `(status, body)`.
async fn get(addr: &str, target: &str) -> (u16, String) {
    http(addr, "GET", target, None).await
}

/// A foreign index on `addr`: 400 `routing_exception` whose reason names the
/// index, the predicted slot and the owner's RESP bind.
async fn refused(addr: &str, method: &str, target: &str, index: &str, owner: &str) {
    let (st, reply) = http(addr, method, target, None).await;
    let reason = format!("index '{index}' maps to slot {} owned by '{owner}'; {HINT}", slot_of(index));
    assert_eq!(st, 400, "{method} {target} on {addr}: {reply}");
    error_is(&reply, 400, ROUTING, &reason);
}

/// One ES write on `addr`: the HTTP status and the `result` word are pinned.
async fn write(addr: &str, method: &str, target: &str, body: &str, status: u16, result: &str) {
    let (st, reply) = http(addr, method, target, Some(body)).await;
    assert_eq!(st, status, "{method} {target}: {reply}");
    assert_eq!(json_of(&reply)["result"], result, "{method} {target}: {reply}");
}

/// `PUT /{index}` with the shared mappings: 200 + acknowledged.
async fn create(addr: &str, index: &str) {
    let (st, reply) = http(addr, "PUT", &format!("/{index}"), Some(MAPPINGS)).await;
    assert_eq!(st, 200, "PUT /{index}: {reply}");
    let v = json_of(&reply);
    assert_eq!(v["acknowledged"], true, "{reply}");
    assert_eq!(v["index"], index, "{reply}");
}

/// `_cat/indices` on `addr` equals the expected table, with and without `?v=true`.
async fn census_is(addr: &str, rows: &[(&str, u64)]) {
    let want = census(rows);
    for target in ["/_cat/indices", "/_cat/indices?v=true"] {
        let (st, body) = get(addr, target).await;
        assert_eq!((st, body.as_str()), (200, want.as_str()), "{target} on {addr}");
    }
}

/// Poll `GET /_cluster/health` until the body equals `want` (<= `secs`).
async fn health_becomes(addr: &str, want: &str, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let (st, body) = get(addr, "/_cluster/health").await;
        if (st, body.as_str()) == (200, want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "health on {addr} never reached {want}; last {st} {body}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- scenario ----------------------------------------------------------

#[tokio::test]
async fn es_health_init_census() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    assert_eq!(crc16(b"key") % 16384, 12539, "model: Redis slot of 'key'");
    assert_eq!(crc16(b"foo") % 16384, 12182, "model: Redis slot of 'foo'");

    // a) Two real nodes with the ES front; the joiner follows node0's raft-http.
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (n0, _) = front_world(TAG, &spec).await;
    let join = n0.node.http.clone();
    let mut n1 = spawn_front_node(&world_dir("eshealth174b"), 1, false, Some(&join), &spec);
    wait_fronts(&mut n1, false, true, false).await;
    let nodes = [n0.node, n1.node];
    let es = [nodes[0].es.clone(), nodes[1].es.clone()];

    // b) Pre-INIT on BOTH nodes: YELLOW with one node, an EMPTY census (header
    //    only) and `GET /` naming the node's own RESP bind.
    for i in 0..2 {
        let (st, body) = get(&es[i], "/_cluster/health").await;
        assert_eq!((st, body.as_str()), (200, YELLOW), "pre-INIT health on node{i}");
        let (st, body) = get(&es[i], "/_cat/indices").await;
        assert_eq!((st, body.as_str()), (200, CAT_HEAD), "pre-INIT census on node{i}");
        let (st, body) = get(&es[i], "/").await;
        assert_eq!(st, 200, "GET / on node{i}: {body}");
        let v = json_of(&body);
        assert_eq!(v["name"], nodes[i].resp.as_str(), "GET / name on node{i}: {body}");
        assert_eq!(v["cluster_name"], "rdb", "{body}");
    }

    // c) INIT in node order: topology converges, health flips to GREEN x2 nodes.
    let leader = wait_leader(&nodes, 30).await;
    let peers = [nodes[0].resp.clone(), nodes[1].resp.clone()];
    cluster_init(&nodes[leader], &peers).await;
    wait_cluster_nodes_list_all(&nodes, &peers, 10).await;
    for addr in &es {
        health_becomes(addr, GREEN, 10).await;
    }
    let lead = nodes[leader].resp.clone();
    let (ea, eb) = (es[0].as_str(), es[1].as_str());
    let (addr_a, addr_b) = (nodes[0].resp.as_str(), nodes[1].resp.as_str());

    // d) Placement by the band rule: three names owned by node0 (two inside
    //    band 0 and one exactly ON the inclusive edge slot 8192) and two owned
    //    by node1 (band 1 and the slot right above the edge). RESP agrees.
    let band0 = |s: u16| owner_of(s, 2) == 0;
    let band1 = |s: u16| owner_of(s, 2) == 1;
    let (a1, a2, lo) = (pick(band0, 0), pick(band0, 1), pick(|s| s == EDGE, 0));
    let (b1, hi) = (pick(band1, 0), pick(|s| s == EDGE + 1, 0));
    assert_eq!((slot_of(&lo), slot_of(&hi)), (EDGE, EDGE + 1), "model edge slots");
    for name in [&a1, &a2, &lo, &b1, &hi] {
        let got = int_of(&r(&lead, &[b"cluster", b"keyslot", name.as_bytes()]).await);
        assert_eq!(got, i64::from(slot_of(name)), "CLUSTER KEYSLOT {name}");
    }
    let (a1, a2, lo, b1, hi) = (a1.as_str(), a2.as_str(), lo.as_str(), b1.as_str(), hi.as_str());

    // e) Create on the owners; a foreign create is a routing refusal both ways.
    refused(ea, "PUT", &format!("/{b1}"), b1, addr_b).await;
    refused(eb, "PUT", &format!("/{a1}"), a1, addr_a).await;
    for name in [a1, a2, lo] {
        create(ea, name).await;
    }
    for name in [b1, hi] {
        create(eb, name).await;
    }
    census_is(ea, &[(a1, 0), (a2, 0), (lo, 0)]).await;
    census_is(eb, &[(b1, 0), (hi, 0)]).await;

    // f) Documents on the owners: create 201, update 200 keeps the count,
    //    delete 200 decrements, re-delete 200 "not_found" (ES wire), a GET hit
    //    carries the replaced source and a miss is 404 `found:false`.
    write(ea, "POST", &format!("/{a1}/_doc/1"), DOC_A, 201, "created").await;
    write(ea, "PUT", &format!("/{a1}/_doc/2"), DOC_B, 201, "created").await;
    census_is(ea, &[(a1, 2), (a2, 0), (lo, 0)]).await;
    write(ea, "PUT", &format!("/{a1}/_doc/1"), DOC_C, 200, "updated").await;
    census_is(ea, &[(a1, 2), (a2, 0), (lo, 0)]).await;
    write(ea, "DELETE", &format!("/{a1}/_doc/2"), "", 200, "deleted").await;
    write(ea, "DELETE", &format!("/{a1}/_doc/2"), "", 200, "not_found").await;
    for (addr, target) in [
        (ea, format!("/{a2}/_doc/1")),
        (ea, format!("/{lo}/_doc/1")),
        (eb, format!("/{b1}/_doc/1")),
        (eb, format!("/{b1}/_doc/2")),
        (eb, format!("/{hi}/_doc/1")),
    ] {
        write(addr, "POST", &target, DOC_B, 201, "created").await;
    }
    let (st, body) = get(ea, &format!("/{a1}/_doc/1")).await;
    assert_eq!(st, 200, "{body}");
    let v = json_of(&body);
    assert_eq!((v["found"].clone(), v["_id"].clone()), (true.into(), "1".into()), "{body}");
    assert_eq!((v["_index"].clone(), v["_source"]["qty"].clone()), (a1.into(), 9.into()), "{body}");
    let (st, body) = get(ea, &format!("/{a1}/_doc/2")).await;
    assert_eq!((st, json_of(&body)["found"].clone()), (404, false.into()), "{body}");
    let (st, body) = http(eb, "POST", &format!("/{b1}/_search"), Some(MATCH_ALL)).await;
    assert_eq!(st, 200, "{body}");
    let v = json_of(&body);
    assert_eq!((v["hits"]["total"]["value"].clone(), v["hits"]["hits"].as_array().map(Vec::len)),
               (2.into(), Some(2)), "{body}");

    // g) Hot block: both censuses (x2 forms) + four counts + four routing refusals.
    let t = scenario_common::perf::now();
    census_is(ea, &[(a1, 1), (a2, 1), (lo, 1)]).await;
    census_is(eb, &[(b1, 2), (hi, 1)]).await;
    for (addr, name, want) in [(ea, a1, 1), (ea, a2, 1), (eb, b1, 2), (eb, hi, 1)] {
        let (st, body) = get(addr, &format!("/{name}/_count")).await;
        assert_eq!((st, body), (200, format!("{{\"count\":{want}}}")), "_count {name} on {addr}");
    }
    refused(ea, "PUT", &format!("/{b1}/_doc/9"), b1, addr_b).await;
    refused(ea, "GET", &format!("/{b1}/_doc/1"), b1, addr_b).await;
    refused(ea, "GET", &format!("/{b1}/_search"), b1, addr_b).await;
    refused(ea, "GET", &format!("/{hi}/_count"), hi, addr_b).await;
    scenario_common::perf::within("eshealth/census", t, Duration::from_secs(2));

    // h) `_cat` is not reserved below the two real routes: `/_cat/health` and
    //    `/_cat/indices/x` fall to `/{index}` -- the slot owner of index '_cat'
    //    answers 404 no_handler_found_exception, the other node a routing refusal.
    let cat_owner = owner_of(slot_of("_cat"), 2);
    let (es_owner, es_other, addr_owner) =
        (es[cat_owner].as_str(), es[1 - cat_owner].as_str(), nodes[cat_owner].resp.as_str());
    for target in ["/_cat/health", "/_cat/indices/x"] {
        let (st, body) = get(es_owner, target).await;
        let want = format!("no handler found for [GET {target}]");
        assert_eq!(st, 404, "{target} on the _cat owner: {body}");
        error_is(&body, 404, NO_HANDLER, &want);
        refused(es_other, "GET", target, "_cat", addr_owner).await;
    }
    scenario_common::perf::scenario("eshealth", t0);
}
