//! Process-level e2e for the ES front's two request-gating layers:
//! the Bearer auth gate and the remote-slot `routing_exception`.
//!
//! Auth (`src/es/http.rs`): a non-empty `es_token` turns EVERY route
//! -- meta endpoints included -- into the 401 `security_exception`
//! envelope until `Authorization: Bearer <token>` matches verbatim.
//! `spawn_node_es` bakes `es_token: ""` into the yaml, so this test
//! kills the first child, swaps that line for a FAKE token (serde
//! rejects duplicate yaml keys, so the empty line must be REPLACED,
//! not appended to) and respawns on the same ports + store.
//!
//! Routing (`src/es/write.rs::slot_check`): on a 2-node cluster every
//! `/{index}` route is checked against the equal-split slot bands
//! BEFORE any store access; a remote slot answers 400
//! `routing_exception` naming the owning RESP addr (the ES fan-out
//! client's MOVED equivalent) while the owner serves the very same
//! request.

mod common;
mod es_common;

use std::time::{Duration, Instant};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{
    cluster_init, cmd_one_shot, contains_bytes, spawn_node_es, wait_cluster_nodes_list_all,
    wait_leader, wait_resp_ready, ProcNode, TOKEN,
};
use es_common::{http, json_body};

/// FAKE Bearer token written into the node's yaml (never a real secret).
const ES_TOKEN: &str = "e2e-es-token";

/// The exact body `reply::error(401, "security_exception", ..)` emits
/// (serde_json keeps insertion order, so the string is stable).
const UNAUTHORIZED_BODY: &str = concat!(
    r#"{"error":{"root_cause":[{"type":"security_exception","#,
    r#""reason":"missing or invalid bearer credentials"}],"#,
    r#""type":"security_exception","reason":"missing or invalid bearer credentials"},"#,
    r#""status":401}"#
);

/// Minimal mappings (text + keyword) for the index-lifecycle puts.
const MAPPINGS: &str =
    r#"{"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"}}}}"#;

const DOC: &str = r#"{"title":"bearer auth gate","tag":"auth"}"#;

/// One HTTP round trip with an OPTIONAL Authorization header value
/// (e.g. `Some("Bearer tok")`); `(0, "")` on connect/read failure,
/// like `es_common::http` (which never sends the header). Local to
/// this file so `es_common` keeps its 4-arg shape.
async fn http_auth(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    auth: Option<&str>,
) -> (u16, String) {
    let body = body.unwrap_or("");
    let auth_line = match auth {
        Some(v) => format!("Authorization: {v}\r\n"),
        None => String::new(),
    };
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: e2e\r\n{auth_line}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => return (0, String::new()),
    };
    if sock.write_all(req.as_bytes()).await.is_err() {
        return (0, String::new());
    }
    let mut buf = Vec::new();
    match tokio::time::timeout(Duration::from_secs(10), sock.read_to_end(&mut buf)).await {
        Ok(Ok(_)) => {}
        _ => return (0, String::new()),
    }
    let raw = String::from_utf8_lossy(&buf).into_owned();
    let status = raw
        .split(' ')
        .nth(1)
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    (
        status,
        raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string(),
    )
}

/// Poll GET / (no header) until it answers 200; `(0, "")` means the
/// es listener is not accepting yet. Only used on token-LESS nodes.
async fn wait_es_root(node: &ProcNode, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let (status, body) = http(&node.es, "GET", "/", None).await;
        if status == 200 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "es {} never answered 200 on /; last=({status}, {body})\n{}",
            node.es,
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn bearer_gate_covers_the_whole_surface() {
    let dir = std::env::temp_dir().join(format!("rdb-es-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_node_es(&dir, 0, true, None);

    // The token goes into the yaml only after the first launch: kill,
    // swap `es_token: ""` for the fake one, respawn (same ports/store).
    node.kill_now();
    let raw = std::fs::read_to_string(&node.config_path).expect("read back conf.yaml");
    let swapped = raw.replace("es_token: \"\"", &format!("es_token: \"{ES_TOKEN}\""));
    assert_eq!(
        swapped.len(),
        raw.len() + ES_TOKEN.len(),
        "one line swapped: {raw}"
    );
    std::fs::write(&node.config_path, swapped).expect("write conf.yaml with es_token");
    node.respawn();
    wait_resp_ready(&mut node, 30).await;

    // Readiness = the gate itself: http() sends no Authorization
    // header, so 401 means the es listener + token are both up
    // (status 0 = listener not accepting yet, keep polling).
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, _) = http(&node.es, "GET", "/", None).await;
        if status == 401 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "gated es port never answered 401; last status {status}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // No header -> the exact 401 envelope pinned by http_tests.rs.
    let (status, body) = http_auth(&node.es, "GET", "/", None, None).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(body, UNAUTHORIZED_BODY, "exact 401 error envelope");
    // Wrong bearer -> the SAME envelope (simple verbatim compare).
    let (status, wrong) = http_auth(&node.es, "GET", "/", None, Some("Bearer not-the-token")).await;
    assert_eq!((status, wrong.as_str()), (401, UNAUTHORIZED_BODY));
    // Basic scheme instead of Bearer -> still 401.
    let (status, _) = http_auth(&node.es, "GET", "/", None, Some("Basic dXNlcjpwYXNz")).await;
    assert_eq!(status, 401);

    // Correct bearer -> 200 root with the tagline.
    let auth = format!("Bearer {ES_TOKEN}");
    let (status, body) = http_auth(&node.es, "GET", "/", None, Some(&auth)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["tagline"], "You Know, for Search");

    // The gate covers the API surface, not just the meta endpoints:
    // index create, doc write and search all 401 without the header.
    let (status, no) = http_auth(&node.es, "PUT", "/books", Some(MAPPINGS), None).await;
    assert_eq!((status, no.as_str()), (401, UNAUTHORIZED_BODY));
    let (status, no) = http_auth(&node.es, "PUT", "/books/_doc/1", Some(DOC), None).await;
    assert_eq!((status, no.as_str()), (401, UNAUTHORIZED_BODY));
    let (status, no) = http_auth(&node.es, "POST", "/books/_search", Some(DOC), None).await;
    assert_eq!((status, no.as_str()), (401, UNAUTHORIZED_BODY));
    let (status, no) = http_auth(&node.es, "GET", "/_cluster/health", None, None).await;
    assert_eq!(status, 401, "{no}");

    // With the header the same requests succeed end to end.
    let (status, body) = http_auth(&node.es, "PUT", "/books", Some(MAPPINGS), Some(&auth)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["acknowledged"], true);
    let (status, body) = http_auth(&node.es, "PUT", "/books/_doc/1", Some(DOC), Some(&auth)).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(json_body(&body)["result"], "created");
    let q = r#"{"query":{"match":{"title":"bearer"}}}"#;
    let (status, body) = http_auth(&node.es, "POST", "/books/_search", Some(q), Some(&auth)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        json_body(&body)["hits"]["total"]["value"],
        1,
        "doc survived the gate"
    );
}

/// The exact `slot_check` reason text (owner addr = the owner's RESP
/// bind, the address ES fan-out clients must target next).
fn routing_reason(index: &str, slot: u16, owner_resp: &str) -> String {
    format!(
        "index '{index}' maps to slot {slot} owned by '{owner_resp}'; \
         ES fan-out clients must target the owning node (RESP MOVED equivalent)"
    )
}

/// Equal 2-node bands: 16384 / 2 = 8192 slots per node, stable_addrs
/// order [resp0, resp1] -> slots <= 8192 on node0, the rest (the last
/// node absorbs the remainder) on node1.
fn band_of(slot: u16) -> usize {
    usize::from(slot > 8192)
}

/// A short single-letter index name whose slot lands in `band`
/// (brute force; a 26-letter miss is ~2^-26 per band).
fn find_index_in_band(band: usize) -> (String, u16) {
    for c in b'a'..=b'z' {
        let name = (c as char).to_string();
        let (slot, _) = rdb::hash::slot_with_prefix(name.as_bytes());
        if band_of(slot) == band {
            return (name, slot);
        }
    }
    panic!("no single-letter index name landed in band {band}");
}

/// Wait until `node`'s CACHED topology routes `key`'s slot to
/// `owner_resp`: the RESP layer answers `-MOVED <slot> <owner>` for a
/// plain GET. `cluster nodes` listing convergence (raft FSM) runs ahead
/// of the per-process 3s topology resync ticker, and with a not-yet-
/// synced (empty) stable list the router's Go-compatible fallback is
/// Local -- the ES slot_check would then wrongly serve the write.
async fn wait_routes_remote(node: &ProcNode, key: &str, owner_resp: &str, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(&node.resp, TOKEN, &[b"GET", key.as_bytes()]).await;
        if r.starts_with(b"-MOVED ") && contains_bytes(&r, owner_resp.as_bytes()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "node {} never MOVED {key} to {owner_resp}; last={r:?}\n{}",
            node.resp,
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn remote_slot_requests_answer_routing_exception() {
    let dir = std::env::temp_dir().join(format!("rdb-es-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    // 2-node cluster, BOTH nodes with the es front enabled: node0
    // bootstraps and must lead before the joiner spawns (the HTTP
    // /join is a single attempt), then CLUSTER INIT + convergence.
    let mut nodes = Vec::new();
    let mut n0 = spawn_node_es(&dir, 0, true, None);
    wait_resp_ready(&mut n0, 90).await;
    nodes.push(n0);
    assert_eq!(
        wait_leader(&nodes, 120).await,
        0,
        "node0 must lead before joins\n{}",
        common::all_ctx(&nodes)
    );
    let join = nodes[0].http.clone();
    let mut n1 = spawn_node_es(&dir, 1, false, Some(&join));
    wait_resp_ready(&mut n1, 90).await;
    nodes.push(n1);
    let leader = wait_leader(&nodes, 120).await;
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 90).await;
    for n in &nodes {
        wait_es_root(n, 15).await;
    }

    // One index per band: n0_idx is owned by node0, n1_idx by node1.
    let (n0_idx, n0_slot) = find_index_in_band(0);
    let (n1_idx, n1_slot) = find_index_in_band(1);
    assert_eq!(
        (band_of(n0_slot), band_of(n1_slot)),
        (0, 1),
        "{n0_idx}@{n0_slot} {n1_idx}@{n1_slot}"
    );

    // The cached topology must route both bands remotely on BOTH nodes
    // before any ES assertion (a pre-ticker node still serves Local).
    wait_routes_remote(&nodes[1], &n0_idx, &nodes[0].resp, 90).await;
    wait_routes_remote(&nodes[0], &n1_idx, &nodes[1].resp, 90).await;

    // Non-owner (node1) refuses the node0-owned index create: 400 +
    // the exact routing_exception envelope naming node0's resp bind.
    let (status, body) = http(&nodes[1].es, "PUT", &format!("/{n0_idx}"), Some(MAPPINGS)).await;
    assert_eq!(status, 400, "{body}");
    let v = json_body(&body);
    assert_eq!(v["error"]["type"], "routing_exception", "{v}");
    assert_eq!(v["error"]["root_cause"][0]["type"], "routing_exception");
    assert_eq!(
        v["error"]["root_cause"][0]["reason"],
        routing_reason(&n0_idx, n0_slot, &nodes[0].resp),
        "reason names the owning resp addr"
    );
    assert_eq!(v["status"], 400);

    // Doc writes on the non-owner hit the SAME gate (slot_check runs
    // before any store access, so the missing index is never reached).
    let (status, body) = http(&nodes[1].es, "PUT", &format!("/{n0_idx}/_doc/1"), Some(DOC)).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        json_body(&body)["error"]["root_cause"][0]["reason"],
        routing_reason(&n0_idx, n0_slot, &nodes[0].resp)
    );

    // The owner serves the very same create + doc write fine.
    let (status, body) = http(&nodes[0].es, "PUT", &format!("/{n0_idx}"), Some(MAPPINGS)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["acknowledged"], true);
    let (status, body) = http(&nodes[0].es, "PUT", &format!("/{n0_idx}/_doc/1"), Some(DOC)).await;
    assert_eq!(status, 201, "{body}");

    // Even a READ of an index that EXISTS (on the owner) is routed,
    // not proxied: node1 answers 400 routing_exception, not 404/200.
    let (status, body) = http(&nodes[1].es, "GET", &format!("/{n0_idx}"), None).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        json_body(&body)["error"]["root_cause"][0]["reason"],
        routing_reason(&n0_idx, n0_slot, &nodes[0].resp)
    );

    // Mirror direction: node0 refuses the node1-owned band member.
    let (status, body) = http(&nodes[0].es, "PUT", &format!("/{n1_idx}"), Some(MAPPINGS)).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        json_body(&body)["error"]["root_cause"][0]["reason"],
        routing_reason(&n1_idx, n1_slot, &nodes[1].resp)
    );
    let (status, body) = http(&nodes[1].es, "PUT", &format!("/{n1_idx}"), Some(MAPPINGS)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["acknowledged"], true);

    // Pinned whole shape once, for the record (both envelope halves).
    let (status, body) = http(&nodes[0].es, "GET", &format!("/{n1_idx}"), None).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        json_body(&body),
        json!({
            "error": {
                "root_cause": [{
                    "type": "routing_exception",
                    "reason": routing_reason(&n1_idx, n1_slot, &nodes[1].resp),
                }],
                "type": "routing_exception",
                "reason": routing_reason(&n1_idx, n1_slot, &nodes[1].resp),
            },
            "status": 400,
        }),
        "the full 400 envelope"
    );
}
