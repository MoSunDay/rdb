//! Lite DLQ e2e (process-level, real binary over RESP + /metrics): the
//! MAXDELIVERY dead-letter contract of plans/2026-10-06-mq-gap
//! 01-engine-reliability.md §3 vs checklist 06-e2e-matrix.md §2.1 --
//! over-limit transfer + atomic trio + repeat claims + ordered head +
//! DLQ-as-plain-stream + syntax gates + kill -9 + gauge + 默认无重投;
//! in-process sweeps live in lite_redeliver_e2e.rs (400-line gate 06 §1).

mod common;

use common::lite::{cmd_full_reply, pel_rows, text};
use common::{cmd_one_shot, spawn_node, wait_resp_ready, ProcNode, TOKEN};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Fresh node on a unique temp dir -> (node, resp addr). The node must
/// stay bound (`_node`: Drop kills it).
async fn boot(tag: &str) -> (ProcNode, String) {
    let dir = std::env::temp_dir().join(format!("rdb-lite-dlq-{tag}-{}", std::process::id()));
    let mut node = spawn_node(&dir, 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    let resp = node.resp.clone();
    (node, resp)
}

async fn t(a: &str, args: &[&[u8]]) -> String {
    text(&cmd_one_shot(a, TOKEN, args).await)
}

async fn full(a: &str, args: &[&[u8]]) -> String {
    text(&cmd_full_reply(a, TOKEN, args, 200).await)
}

/// XGROUP CREATE <s> <g> 0-0 MKSTREAM <extra..>; asserts +OK.
async fn gcreate(a: &str, s: &[u8], g: &[u8], extra: &[&[u8]]) {
    let mut args: Vec<&[u8]> = vec![b"xgroup", b"create", s, g, b"0-0", b"MKSTREAM"];
    args.extend_from_slice(extra);
    assert_eq!(t(a, &args).await, "+OK", "xgroup create {s:?}/{g:?}");
}

/// Same shape as `gcreate` but the engine must reject it with -ERR.
async fn gcreate_err(a: &str, s: &[u8], extra: &[&[u8]]) {
    let mut args: Vec<&[u8]> = vec![b"xgroup", b"create", s, b"g", b"0-0", b"MKSTREAM"];
    args.extend_from_slice(extra);
    let r = t(a, &args).await;
    assert!(r.starts_with("-ERR"), "xgroup create {s:?} must fail: {r}");
}

async fn xadd(a: &str, s: &[u8], id: &str, v: &str) {
    let r = t(a, &[b"xadd", s, id.as_bytes(), b"f", v.as_bytes()]).await;
    assert!(r.contains(id), "xadd {id}: {r}");
}

async fn read_new(a: &str, s: &[u8], g: &[u8], c: &[u8]) -> String {
    full(a, &[b"xreadgroup", b"group", g, c, b"streams", s, b">"]).await
}

/// XCLAIM <s> <g> <c> 0 <id> that must DELIVER (a legal bump).
async fn claim_delivers(a: &str, s: &[u8], g: &[u8], c: &[u8], id: &str) {
    let r = full(a, &[b"xclaim", s, g, c, b"0", id.as_bytes()]).await;
    assert!(r.contains(id), "claim {id} must deliver: {r}");
}

/// XCLAIM at maxdelivery transfers to the DLQ; reply carries no entry.
async fn claim_transfers(a: &str, s: &[u8], g: &[u8], c: &[u8], id: &str) {
    let r = full(a, &[b"xclaim", s, g, c, b"0", id.as_bytes()]).await;
    assert_eq!(r, "*0\r\n", "claim {id} must transfer, not deliver");
}

/// Group `g` (MAXDELIVERY 2 + `extra`) with one message driven to a
/// completed transfer: `>` #1, legal claim #2, over-limit claim #3.
async fn group_with_transfer(a: &str, s: &[u8], v: &str, extra: &[&[u8]]) {
    let mut opts: Vec<&[u8]> = vec![b"MAXDELIVERY", b"2"];
    opts.extend_from_slice(extra);
    gcreate(a, s, b"g", &opts).await;
    xadd(a, s, "1-1", v).await;
    assert!(read_new(a, s, b"g", b"c1").await.contains("1-1"));
    claim_delivers(a, s, b"g", b"c2", "1-1").await;
    claim_transfers(a, s, b"g", b"c2", "1-1").await;
}

/// XRANGE <s> full text must contain every needle.
async fn xrange_has(a: &str, s: &[u8], needles: &[&str]) {
    let r = full(a, &[b"xrange", s, b"-", b"+"]).await;
    for n in needles {
        assert!(r.contains(n), "xrange {s:?} missing {n}: {r}");
    }
}

/// (id, consumer, deliveries) of `XPENDING <s> <g> - + 10` range rows
/// (the >5 filter drops a `*4\r\n` reply-header artifact at 4 rows).
async fn pending(a: &str, s: &[u8], g: &[u8]) -> Vec<(String, String, u64)> {
    let reply = cmd_full_reply(a, TOKEN, &[b"xpending", s, g, b"-", b"+", b"10"], 200).await;
    pel_rows(&reply)
        .into_iter()
        .filter(|r| r.len() > 5)
        .map(|r| {
            (
                r[1].clone(),
                r[3].clone(),
                r[5].trim_start_matches(':').parse::<u64>().unwrap_or(0),
            )
        })
        .collect()
}

/// Expected-PEL literal for assert_eq against `pending`.
fn rows(v: &[(&str, &str, u64)]) -> Vec<(String, String, u64)> {
    v.iter()
        .map(|(i, c, n)| (i.to_string(), c.to_string(), *n))
        .collect()
}

/// Empty XPENDING summary (all rows gone from the group's PEL).
const PEL0: &str = "*3\r\n:0\r\n$-1\r\n$-1\r\n";

async fn pel_empty(a: &str, s: &[u8], g: &[u8]) {
    assert_eq!(full(a, &[b"xpending", s, g]).await, PEL0);
}

/// Labeled bulk value in a flat XINFO GROUPS array (label, $len, value
/// positional per src/lite/info.rs groups_info).
fn flat_field(reply: &str, label: &str) -> String {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let i = toks
        .iter()
        .position(|x| *x == label)
        .unwrap_or_else(|| panic!("no '{label}' in {reply}"));
    toks[i + 2].to_string()
}

async fn committed_id(a: &str, s: &[u8]) -> String {
    flat_field(&full(a, &[b"xinfo", b"groups", s]).await, "committed-id")
}

/// First entry id of the single stream in an XREADGROUP reply
/// (stream bulk, *N, *2, $len, id -> id at stream+4).
fn first_entry_id(reply: &str, stream: &str) -> String {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let i = toks
        .iter()
        .position(|x| *x == stream)
        .unwrap_or_else(|| panic!("no '{stream}' in {reply}"));
    toks[i + 4].to_string()
}

/// One raw HTTP/1.0 GET on the monitor port; read to connection close.
async fn http_get(addr: &str, path: &str) -> Vec<u8> {
    let Ok(mut sock) = TcpStream::connect(addr).await else {
        return b"<CONN-ERR>".to_vec();
    };
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n");
    if sock.write_all(req.as_bytes()).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    buf
}

/// Summed value of gauge `name` across its (optionally labeled) lines.
fn gauge(body: &[u8], name: &str) -> Option<u64> {
    let mut sum = 0u64;
    let mut seen = false;
    for line in String::from_utf8_lossy(body).lines() {
        if let Some(rest) = line.strip_prefix(name) {
            if rest.starts_with(' ') || rest.starts_with('{') {
                seen = true;
                sum += rest
                    .split_whitespace()
                    .last()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
            }
        }
    }
    seen.then_some(sum)
}

/// Poll /metrics until rdb_lite_dlq_depth == want (200ms refresh).
async fn wait_dlq_depth(node: &ProcNode, want: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let body = http_get(&node.monitor, "/metrics").await;
        if gauge(&body, "rdb_lite_dlq_depth") == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rdb_lite_dlq_depth never reached {want}; last scrape:\n{}",
            String::from_utf8_lossy(&body)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Transfer trigger + threshold boundary + atomic trio (PEL row gone,
/// DLQ entry, committed watermark) + transferred ids never resurface.
#[tokio::test]
async fn transfer_triggers_at_maxdelivery_and_advances_watermark() {
    let (_node, a) = boot("t1").await;
    gcreate(&a, b"d/q0", b"g", &[b"MAXDELIVERY", b"2"]).await;
    for i in 1..=3 {
        xadd(&a, b"d/q0", &format!("1-{i}"), &format!("v{i}")).await;
    }
    // `>` delivery #1 for all three; 1-1 is finished immediately.
    assert!(read_new(&a, b"d/q0", b"g", b"c1").await.contains("1-3"));
    assert_eq!(t(&a, &[b"xack", b"d/q0", b"g", b"1-1"]).await, ":1");
    // Threshold boundary ("差一次时不转移"): REACHING maxdelivery (1 -> 2)
    // is still a legal delivery.
    claim_delivers(&a, b"d/q0", b"g", b"c2", "1-2").await;
    claim_delivers(&a, b"d/q0", b"g", b"c2", "1-3").await;
    assert_eq!(
        pending(&a, b"d/q0", b"g").await,
        rows(&[("1-2", "c2", 2), ("1-3", "c2", 2)])
    );
    // The attempt that would EXCEED (2 -> 3 > 2) leaves the group
    // atomically instead: (a) the PEL row vanished ...
    claim_transfers(&a, b"d/q0", b"g", b"c3", "1-3").await;
    assert_eq!(pending(&a, b"d/q0", b"g").await, rows(&[("1-2", "c2", 2)]));
    // (b) the DLQ entry carries the original payload + trace fields ...
    xrange_has(&a, b"d/q0/dlq", &["1-3", "v3"]).await;
    xrange_has(&a, b"d/q0/dlq", &["__dlq_group", "__dlq_consumer"]).await;
    xrange_has(&a, b"d/q0/dlq", &["__dlq_times", "__dlq_src"]).await;
    assert_eq!(t(&a, &[b"xlen", b"d/q0/dlq"]).await, ":1");
    // (c) the watermark crossed the transfer, but only as far as the
    // live prefix reaches (1-2 is still pending).
    assert_eq!(committed_id(&a, b"d/q0").await, "1-1");
    // Transferring the survivor drains the PEL and crosses both ids.
    claim_transfers(&a, b"d/q0", b"g", b"c3", "1-2").await;
    pel_empty(&a, b"d/q0", b"g").await;
    assert_eq!(committed_id(&a, b"d/q0").await, "1-3");
    assert_eq!(t(&a, &[b"xlen", b"d/q0/dlq"]).await, ":2");
    // The transferred ids never resurface through `>`.
    xadd(&a, b"d/q0", "1-4", "v4").await;
    let again = read_new(&a, b"d/q0", b"g", b"c9").await;
    assert!(
        again.contains("1-4") && !again.contains("1-2") && !again.contains("1-3"),
        "post-transfer `>`: {again}"
    );
}

/// A repeat claim of an already-transferred id neither delivers nor
/// appends another DLQ copy ("重复 claim 不双转").
#[tokio::test]
async fn repeat_claim_does_not_double_transfer() {
    let (_node, a) = boot("t2").await;
    group_with_transfer(&a, b"d/q10", "poison", &[]).await;
    assert_eq!(t(&a, &[b"xlen", b"d/q10/dlq"]).await, ":1");
    // Re-claiming the same id twice more: still no entry, depth stays 1.
    claim_transfers(&a, b"d/q10", b"g", b"c3", "1-1").await;
    claim_transfers(&a, b"d/q10", b"g", b"c4", "1-1").await;
    assert_eq!(t(&a, &[b"xlen", b"d/q10/dlq"]).await, ":1");
    pel_empty(&a, b"d/q10", b"g").await;
}

/// ORDERED groups transfer only the PEL head: over-limit attempts on
/// the tail are inert, and transferring the head leaves it pending.
#[tokio::test]
async fn ordered_group_transfers_only_head() {
    let (_node, a) = boot("t3").await;
    gcreate(
        &a,
        b"d/q11",
        b"g",
        &[b"ORDERED", b"INFLIGHT", b"2", b"MAXDELIVERY", b"2"],
    )
    .await;
    xadd(&a, b"d/q11", "1-1", "v1").await;
    xadd(&a, b"d/q11", "1-2", "v2").await;
    let read = read_new(&a, b"d/q11", b"g", b"c1").await;
    assert!(
        read.contains("1-1") && read.contains("1-2"),
        "both pending: {read}"
    );
    // Non-head over-limit attempts are inert, whichever consumer asks.
    for c in ["c2", "c3", "c4"] {
        assert_eq!(
            full(&a, &[b"xclaim", b"d/q11", b"g", c.as_bytes(), b"0", b"1-2"]).await,
            "*0\r\n",
            "tail claim by {c} ignored"
        );
    }
    assert_eq!(t(&a, &[b"xlen", b"d/q11/dlq"]).await, ":0");
    assert_eq!(
        pending(&a, b"d/q11", b"g").await,
        rows(&[("1-1", "c1", 1), ("1-2", "c1", 1)])
    );
    // Head to the limit (times 2) still delivers; the next head attempt
    // transfers ONLY the head.
    claim_delivers(&a, b"d/q11", b"g", b"c2", "1-1").await;
    claim_transfers(&a, b"d/q11", b"g", b"c2", "1-1").await;
    assert_eq!(pending(&a, b"d/q11", b"g").await, rows(&[("1-2", "c1", 1)]));
    let dlq = full(&a, &[b"xrange", b"d/q11/dlq", b"-", b"+"]).await;
    assert!(
        dlq.contains("1-1") && !dlq.contains("1-2"),
        "only the head transferred: {dlq}"
    );
}

/// The DLQ stream is a plain Lite stream ("DLQ 可独立消费"): an
/// independent group consumes it with `>` and XACKs; the depth gauge
/// rises on transfer and falls when the dead letter leaves the DLQ.
#[tokio::test]
async fn dlq_is_independently_consumable() {
    let (node, a) = boot("t4").await;
    group_with_transfer(&a, b"d/q12", "dead", &[]).await;
    // Gauge: the transfer made the (single) DLQ depth 1.
    wait_dlq_depth(&node, 1).await;
    // Independent group on the DLQ stream itself (transfer created it).
    assert_eq!(
        t(&a, &[b"xgroup", b"create", b"d/q12/dlq", b"dg", b"0-0"]).await,
        "+OK"
    );
    let rd = read_new(&a, b"d/q12/dlq", b"dg", b"dc").await;
    assert!(
        rd.contains("__dlq_src") && rd.contains("1-1"),
        "dlq consumed: {rd}"
    );
    let id = first_entry_id(&rd, "d/q12/dlq");
    assert_eq!(
        t(&a, &[b"xack", b"d/q12/dlq", b"dg", id.as_bytes()]).await,
        ":1"
    );
    // The DLQ-side traffic never writes back to the business stream.
    assert_eq!(t(&a, &[b"xlen", b"d/q12"]).await, ":1");
    // Depth falls only when the dead letter leaves the DLQ stream.
    assert_eq!(t(&a, &[b"xdel", b"d/q12/dlq", id.as_bytes()]).await, ":1");
    wait_dlq_depth(&node, 0).await;
}

/// Explicit `DLQ <name>` overrides the default; `DLQ` without
/// `MAXDELIVERY` and `MAXDELIVERY 0` are syntax errors; 1 is the floor.
#[tokio::test]
async fn explicit_dlq_name_and_syntax_errors() {
    let (_node, a) = boot("t5").await;
    group_with_transfer(&a, b"d/q13", "x", &[b"DLQ", b"d/other"]).await;
    assert_eq!(t(&a, &[b"xlen", b"d/other"]).await, ":1");
    xrange_has(&a, b"d/other", &["1-1"]).await;
    // The default target was NOT used.
    assert_eq!(t(&a, &[b"xlen", b"d/q13/dlq"]).await, ":0");
    gcreate_err(&a, b"d/q14", &[b"DLQ", b"d/x"]).await; // DLQ requires MAXDELIVERY
    gcreate_err(&a, b"d/q15", &[b"MAXDELIVERY", b"0"]).await; // n >= 1
    gcreate(&a, b"d/q16", b"g", &[b"MAXDELIVERY", b"1"]).await;
}

/// kill -9 + same store/config respawn (scrtips/e2e_scenarios/
/// soak_kill9.sh recipe): a landed transfer stays three-way consistent
/// -- never "PEL row without DLQ entry", never "DLQ entry still pending".
#[tokio::test]
async fn kill9_transfer_is_atomic_and_durable() {
    let (mut node, a) = boot("t6").await;
    group_with_transfer(&a, b"d/q17", "v1", &[]).await;
    node.kill_now();
    node.respawn();
    wait_resp_ready(&mut node, 30).await;
    // No half-transferred state in either direction.
    pel_empty(&a, b"d/q17", b"g").await;
    xrange_has(&a, b"d/q17/dlq", &["1-1", "__dlq_src"]).await;
    assert_eq!(committed_id(&a, b"d/q17").await, "1-1");
    // The restarted gauge still reports the durable depth.
    wait_dlq_depth(&node, 1).await;
    // The transferred id stays gone from the deliverable face.
    xadd(&a, b"d/q17", "1-2", "v2").await;
    let again = read_new(&a, b"d/q17", b"g", b"c9").await;
    assert!(
        again.contains("1-2") && !again.contains("1-1"),
        "post-restart `>`: {again}"
    );
}

/// Redelivery is OFF without `lite.redelivery_idle_ms` (harness config
/// has no `lite:` section): idle rows keep delivery count 1 well past
/// the 200ms sweep rhythm (checklist 2.2 "默认关" anchor).
#[tokio::test]
async fn redelivery_disabled_by_default() {
    let (_node, a) = boot("t7").await;
    gcreate(&a, b"d/q18", b"g", &[]).await;
    xadd(&a, b"d/q18", "1-1", "v").await;
    xadd(&a, b"d/q18", "1-2", "v").await;
    assert!(read_new(&a, b"d/q18", b"g", b"c1").await.contains("1-2"));
    // ~6 sweep rounds of headroom: any background redelivery would have
    // bumped the counters by now.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        pending(&a, b"d/q18", b"g").await,
        rows(&[("1-1", "c1", 1), ("1-2", "c1", 1)]),
        "no sweep without lite.redelivery_idle_ms"
    );
}
