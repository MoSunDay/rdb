//! Multi-store scenario #145 -- observability drill: an operator's shift on a
//! one-node store. `/metrics` (text exposition, `raft_stats{status="Leader"} 1`,
//! `rdb_command_latency` histogram counting exactly the INCRs the shift ran) and
//! the raft control API on the SAME node (`/get` of the seeded `sql_nodes` /
//! `cluster_slots_stable_instances`, 401 for a wrong token on `/join`, `/depart`,
//! `/sql/ts`, `/sql/nodes`) answer as documented while nothing mutates membership;
//! RocksMQ classifies 405/404/400 by path, method and query before a real produce.
//! SURFACES: monitor HTTP + raft control HTTP + RESP + RocksMQ HTTP. One REAL
//! node, RESP + MySQL (bind registered) + RocksMQ + monitor.
//! Deviation: `/sql/ts` is plain text `<start> <end>` (src/sql/tx/global.rs:520-523);
//! `/sql/nodes` w/o `raft` -> 200 `missing raft bind` (src/sql/tx/nodes.rs:199-202);
//! `/join`/`/depart` wrong token -> 401, not the Go fake "ok" (src/rcache/http.rs:12-15,304-306).

use crate::common;
use crate::scenario_common;
use common::TOKEN;
use scenario_common::{FrontSpec, front_world, int_of, json_of, r, rb, rf, rt, stream_ids};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TAG: &str = "obs145";
const COUNTER: &str = "obs145:drill:counter";
const INCRS: i64 = 20;
const CHANNEL: &str = "obs145drill";
const WRONG: &str = "not-the-raft-token";
/// The INCR series as the exposition spells it: the prometheus encoder
/// sorts label names, so `ack` leads (not the type/mode/ack order the
/// collector declares).
const HIST: &str = "rdb_command_latency_count{ack=\"false\",mode=\"incr\",type=\"normal\"}";

/// One raw HTTP/1.1 round trip (`Connection: close`): (status, head
/// block, body). Headers are what this drill is about, so the plain
/// `scenario_common::http` shim (body only) is not enough.
async fn http_raw(addr: &str, method: &str, target: &str, body: &str) -> (u16, String, String) {
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: drill\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut sock = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => return (0, String::new(), String::new()),
    };
    sock.write_all(req.as_bytes()).await.expect("write request");
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), sock.read_to_end(&mut buf)).await;
    read.expect("reply within 10s").expect("read reply");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// Header value of a raw reply (case-insensitive name).
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Value of one exposition sample line (`name{labels} value`), `None`
/// when the series is absent (a HistogramVec child is born on first use).
fn sample(body: &str, series: &str) -> Option<f64> {
    body.lines()
        .find(|l| l.starts_with(&format!("{series} ")))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

/// Poll until `/metrics` shows the raft gauge on `Leader`; returns the
/// (head, body) of that scrape.
async fn metrics_leader(monitor: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, head, body) = http_raw(monitor, "GET", "/metrics", "").await;
        if status == 200 && body.contains("raft_stats{status=\"Leader\"} 1") {
            return (head, body);
        }
        assert!(Instant::now() < deadline, "raft gauge never reached Leader: {status} {body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Parse a `<start> <end>` timestamp lease: both numbers, `end - start`.
fn lease_of(body: &str) -> (u64, u64, u64) {
    let nums: Vec<u64> = body.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    assert_eq!(nums.len(), 2, "lease body: {body:?}");
    (nums[0], nums[1], nums[1] - nums[0])
}

#[tokio::test]
async fn metrics_and_admin_planes_answer_as_documented() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        rocksmq: true,
        ..Default::default()
    };
    let (front, a) = front_world(TAG, &spec).await;
    let node = &front.node;
    let (http, monitor, mq) = (node.http.clone(), node.monitor.clone(), front.rocksmq.clone());

    // a) The Prometheus plane: a 200 text exposition whose raft gauge
    //    settles on Leader; the shift's INCRs land in the histogram.
    //    (A labeled family with no child yet emits nothing at all --
    //    prometheus client semantics -- so the histogram is only
    //    asserted after the first INCR created its series.)
    // The gauge refreshes on a 5s ticker (src/main.rs:178-186), so the
    // first Leader reading is a WAIT, not work: the hot block starts
    // once the gauge is up.
    let (head, body) = metrics_leader(&monitor).await;
    let t = scenario_common::perf::now();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert_eq!(header(&head, "Content-Type"), Some("text/plain; version=0.0.4"), "{head}");
    assert!(body.contains("# TYPE raft_stats gauge"), "{body}");
    assert!(body.contains("# HELP raft_stats raft stats"), "{body}");
    assert_eq!(
        sample(&body, "raft_stats{status=\"Leader\"}").or(Some(-1.0)),
        Some(1.0),
        "the leader label: {body}"
    );
    assert_eq!(sample(&body, "raft_stats{status=\"Follower\"}"), Some(0.0), "{body}");
    let before = sample(&body, HIST).unwrap_or(0.0);
    for _ in 0..INCRS {
        assert!(r(&a, &[b"INCR", COUNTER.as_bytes()]).await.starts_with(b":"), "INCR replied");
    }
    assert_eq!(rb(&a, &[b"GET", COUNTER.as_bytes()]).await, INCRS.to_string(), "the counter itself");
    let (status, head, body) = http_raw(&monitor, "GET", "/metrics", "").await;
    assert_eq!(status, 200, "{head}");
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert_eq!(header(&head, "Content-Type"), Some("text/plain; version=0.0.4"), "{head}");
    assert!(body.contains("# HELP rdb_command_latency rdb command latency(millisecond)"), "{body}");
    assert!(body.contains("# TYPE rdb_command_latency histogram"), "{body}");
    assert!(
        body.contains("rdb_command_latency_bucket{ack=\"false\",mode=\"incr\",type=\"normal\",le=\"5\"}"),
        "incr buckets with the Go bucket edges (5, 30, ..): {body}"
    );
    let after = sample(&body, HIST).unwrap_or_else(|| panic!("incr series absent: {body}"));
    assert!(
        after - before >= INCRS as f64,
        "histogram counted the INCRs: before {before}, after {after}"
    );
    assert!(
        sample(&body, "rdb_command_latency_count{ack=\"false\",mode=\"get\",type=\"normal\"}").unwrap_or(0.0) >= 1.0,
        "the GET is observed under its own mode label: {body}"
    );
    // Anything else on the monitor port is an empty 404.
    let (status, head, body) = http_raw(&monitor, "GET", "/not-metrics", "").await;
    assert_eq!(status, 404, "{head}");
    assert!(body.is_empty(), "404 carries no body: {body:?}");
    assert_eq!(header(&head, "Content-Type"), Some("text/plain; version=0.0.4"), "{head}");
    scenario_common::perf::within("obs145/metrics-scrape", t, Duration::from_secs(2));

    // b) The raft control API on the same node: CLUSTER INIT seeds the
    //    `sql_nodes` registry and the stable-instance list; `/get`
    //    answers FSM values, every mutating route refuses a wrong token.
    common::wait_leader(std::slice::from_ref(node), 60).await;
    assert!(rt(&a, &[b"raft", b"nodes"]).await.contains("[Leader]"), "raft nodes names the role");
    common::cluster_init(node, std::slice::from_ref(&a)).await;
    let t = scenario_common::perf::now();
    let deadline = Instant::now() + Duration::from_secs(30);
    let reg_text = loop {
        let text = rb(&a, &[b"raft", b"get", b"sql_nodes"]).await;
        if text.contains(&node.mysql) && text.contains(&a) {
            break text;
        }
        assert!(Instant::now() < deadline, "sql_nodes never seeded: {text:?}\n{}", node.ctx());
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let reg = json_of(&reg_text);
    assert_eq!(reg.as_object().map(|m| m.len()), Some(1), "exactly one registered node: {reg}");
    let entry = &reg[node.raft.as_str()];
    assert_eq!(entry["raft"], node.raft.as_str(), "{reg}");
    assert_eq!(entry["resp"], a.as_str(), "{reg}");
    assert_eq!(entry["http"], http.as_str(), "{reg}");
    assert_eq!(entry["mysql"], node.mysql.as_str(), "{reg}");
    assert_eq!(entry["sql_rpc"], "", "{reg}");
    let get = |key: &str, token: &str| {
        format!("/get?key={key}&raft-token={token}")
    };
    let (status, _, body) = http_raw(&http, "GET", &get("sql_nodes", TOKEN), "").await;
    let want_reg = format!("{reg_text}\n");
    assert_eq!((status, body.as_str()), (200, want_reg.as_str()), "/get sql_nodes");
    let (status, _, body) = http_raw(&http, "GET", &get("cluster_slots_stable_instances", TOKEN), "").await;
    let want_instances = format!("{a}\n");
    assert_eq!((status, body.as_str()), (200, want_instances.as_str()), "CLUSTER INIT pinned the instances");
    let (status, _, body) = http_raw(&http, "GET", &get("sql_nodes", WRONG), "").await;
    assert_eq!((status, body.as_str()), (200, "\n"), "wrong token on /get: a bare newline");
    let (status, _, body) = http_raw(&http, "GET", &format!("/get?raft-token={TOKEN}"), "").await;
    assert_eq!((status, body.as_str()), (200, ""), "no key: an empty body");
    for route in ["/join", "/depart"] {
        let target = format!("{route}?peerAddress=127.0.0.1:1&raft-token={WRONG}");
        let (status, _, body) = http_raw(&http, "GET", &target, "").await;
        assert_eq!((status, body.as_str()), (401, "unauthorized\n"), "wrong token on {route}");
        let (status, _, body) = http_raw(&http, "GET", &format!("{route}?raft-token={WRONG}"), "").await;
        assert_eq!((status, body.as_str()), (200, "invalid peerAddress\n"), "peerAddress is checked first on {route}");
    }
    // `/sql/ts`: 401 on a wrong token, a plain-text leader lease whose
    //    two calls hand out ADJACENT ranges of the requested size.
    let (status, _, body) = http_raw(&http, "GET", &format!("/sql/ts?n=3&raft-token={WRONG}"), "").await;
    assert_eq!((status, body.as_str()), (401, "unauthorized\n"), "wrong token on /sql/ts");
    let (status, _, body) = http_raw(&http, "GET", &format!("/sql/ts?n=3&raft-token={TOKEN}"), "").await;
    assert_eq!(status, 200, "/sql/ts: {body}");
    let (s1, e1, n1) = lease_of(&body);
    assert!(body.ends_with('\n') && !body.contains('{'), "plain text, not JSON: {body:?}");
    assert_eq!(n1, 3, "lease size: {body:?}");
    assert!(s1 >= 1, "ts 0 is never granted: {body:?}");
    let (status, _, body) = http_raw(&http, "GET", &format!("/sql/ts?n=2&floor={e1}&raft-token={TOKEN}"), "").await;
    assert_eq!(status, 200, "/sql/ts again: {body}");
    assert_eq!(lease_of(&body), (e1, e1 + 2, 2), "the next lease continues the sequence: {body:?}");
    // `/sql/nodes`: 401 on a wrong token; a bind-less call is refused
    //    with 200 and registers NOTHING.
    let (status, _, body) = http_raw(&http, "GET", &format!("/sql/nodes?raft-token={WRONG}"), "").await;
    assert_eq!((status, body.as_str()), (401, "unauthorized\n"), "wrong token on /sql/nodes");
    let (status, _, body) = http_raw(&http, "GET", &format!("/sql/nodes?resp={a}&raft-token={TOKEN}"), "").await;
    assert_eq!((status, body.as_str()), (200, "missing raft bind\n"), "bind-less /sql/nodes");
    let (status, _, body) = http_raw(&http, "GET", "/no-such-route", "").await;
    assert_eq!((status, body.as_str()), (404, "404 page not found\n"), "unknown control path");
    // Nothing above touched the membership or the registry.
    assert_eq!(
        rb(&a, &[b"raft", b"get", b"sql_nodes"]).await,
        reg_text,
        "sql_nodes unchanged by the refused calls"
    );
    assert_eq!(
        rb(&a, &[b"raft", b"get", b"cluster_slots_stable_instances"]).await,
        a,
        "stable instances unchanged"
    );
    assert!(rt(&a, &[b"raft", b"nodes"]).await.contains("[Leader]"), "still the lone leader");

    // c) RocksMQ front: path before method before query, then a real
    //    produce whose id is the stream entry's id.
    let (status, head, body) = http_raw(&mq, "GET", "/produce", "").await;
    assert_eq!((status, body.as_str()), (405, "method not allowed"), "non-POST on a known path");
    assert_eq!(header(&head, "Allow"), Some("POST"), "{head}");
    let (status, _, body) = http_raw(&mq, "POST", "/nope", "").await;
    assert_eq!((status, body.as_str()), (404, "not found"), "unknown path");
    let (status, _, body) = http_raw(&mq, "POST", "/produce?channel=%ZZ", "x").await;
    assert_eq!((status, body.as_str()), (400, "bad query string: malformed % escape"), "{body}");
    let (status, _, body) = http_raw(&mq, "POST", "/produce", "x").await;
    assert_eq!((status, body.as_str()), (400, "missing 'channel' query parameter"), "{body}");
    let (status, _, body) = http_raw(&mq, "POST", &format!("/produce?channel={CHANNEL}"), "").await;
    assert_eq!((status, body.as_str()), (400, "empty body: nothing to produce"), "{body}");
    let (status, head, body) = http_raw(&mq, "POST", &format!("/produce?channel={CHANNEL}"), "drill").await;
    assert_eq!(status, 200, "produce: {head}");
    assert_eq!(header(&head, "Content-Type"), Some("text/plain"), "{head}");
    let id = body.trim().to_string();
    assert!(
        id.split_once('-').is_some_and(|(ms, seq)| !ms.is_empty() && !seq.is_empty()
            && ms.bytes().chain(seq.bytes()).all(|b| b.is_ascii_digit())),
        "id is `<ms>-<seq>`: {id:?}"
    );
    let stream = format!("{CHANNEL}/q0");
    assert_eq!(int_of(&r(&a, &[b"XLEN", stream.as_bytes()]).await), 1, "one entry in {stream}");
    let xrange = rf(&a, &[b"XRANGE", stream.as_bytes(), b"-", b"+"]).await;
    assert_eq!(stream_ids(&xrange), vec![id.clone()], "the id is the entry id: {xrange}");
    assert!(xrange.contains("drill"), "the body rode as the `v` field: {xrange}");
    // The planes still answer after the drill.
    let (status, _, _) = http_raw(&monitor, "GET", "/metrics", "").await;
    assert_eq!(status, 200, "/metrics still up");
    let (status, _, _) = http_raw(&monitor, "GET", "/metricsx", "").await;
    assert_eq!(status, 404, "monitor 404 still empty-typed");
    scenario_common::perf::within("obs145/control-and-mq-probes", t, Duration::from_secs(2));
    scenario_common::perf::scenario("obs145", t0);
}
