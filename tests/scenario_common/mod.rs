//! Multi-store scenario fixture for the `scenario` group crate's
//! `scenario_*_e2e.rs` modules: one REAL rdb binary with ANY subset of
//! the fronts enabled (MySQL, ES HTTP, Kafka, RocksMQ HTTP, S3, SQL 2PC
//! rpc) alongside RESP, the readiness poll, and thin assertion shims.
//! Mounted once by `tests/scenario/main.rs` via
//! `#[path = "../scenario_common/mod.rs"]`; the frozen
//! `tests/common/mod.rs` is reused AS IS and stays untouched (same
//! pattern as `es_common/` and `kafka_front_common/`).
//!
//! Front yaml keys mirror the per-front fixtures: `mysql_bind` +
//! `mysql_user`/`mysql_password` (= common::mysql::PASS), `sql_rpc_bind`
//! (spawn_node_sql), `es_bind`/`es_token` (spawn_node_es),
//! `kafka_bind` (kafka_front_common), `rocksmq_bind` (common::mq),
//! `s3_bind`/`s3_store_path`/`s3_bucket`/`s3_token` (s3::fixture).

#![allow(dead_code)] // each scenario module uses a different subset

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, MutexGuard};

use crate::common::lite::cmd_full_reply_wait;
use crate::common::{
    cmd_one_shot, spawn_node_yaml, wait_mysql_ready, wait_resp_ready, ProcNode, TOKEN,
};

/// Which fronts a scenario node enables (all off by default; RESP is
/// always on) plus verbatim extra yaml (`lite:` sections etc.).
#[derive(Default)]
pub struct FrontSpec {
    pub mysql: bool,
    pub sql_rpc: bool,
    pub es: bool,
    pub kafka: bool,
    pub rocksmq: bool,
    pub s3: bool,
    pub extra_yaml: String,
}

/// Bearer token the S3 front checks (the e2e fixture convention; NOT a
/// real secret).
pub mod codec;

/// Serialize the scenario suites inside the one group crate. Each test
/// spawns a multi-front REAL node and probes free ports; at
/// RUST_TEST_THREADS=4 the probes race (`resp ... not accepting within
/// 30s`, empty replies), while a serial run is both green and faster
/// (203s vs 348s) because the respawn retries vanish.
fn serial_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Hold for the whole test body, before any node spawn.
pub async fn serial_guard() -> MutexGuard<'static, ()> {
    serial_lock().lock().await
}
pub mod kafka; // hand-framed Produce v3 (its own file: keep mod.rs under 400 lines)
pub mod perf; // batch 11 perf gates (perf[label] evidence lines)
pub mod s3_extra; // batch 11 S3 header round trips (Range/HEAD)

pub const S3_TOKEN: &str = "s3-e2e-fake-token-0123456789abcdef0123456789abcdef";

/// One spawned node plus its optional front bind addresses ("" = off).
/// `node.mysql`/`node.es` are backfilled so `common::mysql::connect`
/// and the ES http helpers work unmodified.
pub struct FrontNode {
    pub node: ProcNode,
    pub kafka: String,
    pub rocksmq: String,
    pub s3: String,
}

/// Per-test self-cleaning world root (unique per tag; a leftover from a
/// crashed run of the same pid is removed first).
pub fn world_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rdb-scenario-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scenario world dir");
    dir
}

/// Spawn node `id` with the requested fronts; the EADDRINUSE respawn
/// loop lives in `common::spawn_node_yaml` (extra front ports are
/// pre-reserved here and ride the retry unchanged).
pub fn spawn_front_node(
    dir: &Path,
    id: usize,
    bootstrap: bool,
    join_http: Option<&str>,
    spec: &FrontSpec,
) -> FrontNode {
    let node_dir = dir.join(format!("node{id}"));
    std::fs::create_dir_all(&node_dir).expect("create node dir");
    let [mysql, sql_rpc, es, kafka, rocksmq, s3] = crate::common::free_ports();
    let mut extra = String::new();
    if spec.mysql {
        extra.push_str(&format!(
            "mysql_bind: \"{mysql}\"\nmysql_user: \"root\"\nmysql_password: \"{}\"\n",
            crate::common::mysql::PASS
        ));
    }
    if spec.sql_rpc {
        extra.push_str(&format!("sql_rpc_bind: \"{sql_rpc}\"\n"));
    }
    if spec.es {
        extra.push_str(&format!("es_bind: \"{es}\"\nes_token: \"\"\n"));
    }
    if spec.kafka {
        extra.push_str(&format!("kafka_bind: \"{kafka}\"\n"));
    }
    if spec.rocksmq {
        extra.push_str(&format!("rocksmq_bind: \"{rocksmq}\"\n"));
    }
    if spec.s3 {
        let s3root = node_dir.join("s3data");
        extra.push_str(&format!(
            "s3_bind: \"{s3}\"\ns3_store_path: \"{}\"\ns3_bucket: \"rdb\"\n\
             s3_token: \"{S3_TOKEN}\"\ns3_checkpoint_interval_ms: 500\n",
            s3root.display()
        ));
    }
    extra.push_str(&spec.extra_yaml);
    let mut node = spawn_node_yaml(&node_dir, id, bootstrap, join_http, &extra);
    if spec.mysql {
        node.mysql = mysql;
    }
    if spec.es {
        node.es = es;
    }
    FrontNode {
        node,
        kafka: if spec.kafka { kafka } else { String::new() },
        rocksmq: if spec.rocksmq { rocksmq } else { String::new() },
        s3: if spec.s3 { s3 } else { String::new() },
    }
}

/// A front is up when its port ACCEPTS (the ES front needs one HTTP 200
/// round instead: it accepts before the router is armed).
pub async fn wait_fronts(front: &mut FrontNode, mysql: bool, es: bool, s3: bool) {
    wait_resp_ready(&mut front.node, 30).await;
    if mysql {
        wait_mysql_ready(&front.node, 15).await;
    }
    if es {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (status, _) = http(&front.node.es, "GET", "/", None).await;
            if status == 200 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "es front never up; {}",
                front.node.ctx()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    for (addr, what) in [
        (front.kafka.clone(), "kafka"),
        (front.rocksmq.clone(), "rocksmq"),
    ] {
        if addr.is_empty() {
            continue;
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect(&addr).await.is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{what} front never up; {}",
                front.node.ctx()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if s3 {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect(&front.s3).await.is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "s3 front never up; {}",
                front.node.ctx()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Fresh single bootstrap node with the spec's fronts up and waiting:
/// `(front, resp addr)`. The FrontNode must stay bound (`_front`).
pub async fn front_world(tag: &str, spec: &FrontSpec) -> (FrontNode, String) {
    let mut front = spawn_front_node(&world_dir(tag), 0, true, None, spec);
    wait_fronts(&mut front, spec.mysql, spec.es, spec.s3).await;
    let resp = front.node.resp.clone();
    (front, resp)
}

// ---- RESP shims (raw one-shot + full-drain + assertions) -----------

/// Raw one-shot reply bytes (AUTH folded in).
pub async fn r(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    cmd_one_shot(addr, TOKEN, args).await
}

/// One-shot reply as text (single-line replies).
pub async fn rt(addr: &str, args: &[&[u8]]) -> String {
    crate::common::lite::text(&r(addr, args).await)
}

/// Full-drain reply as text (arrays-of-arrays need the quiet-gap reader).
/// Patient first byte: a just-booted multi-front node can need >250ms
/// before its first reply lands, which the plain 250ms quiet-gap form
/// reads as an empty reply (`no bulk #0 in []`).
pub async fn rf(addr: &str, args: &[&[u8]]) -> String {
    crate::common::lite::text(&cmd_full_reply_wait(addr, TOKEN, args, 250, 10_000).await)
}

/// Assert a `+OK` simple-string reply.
pub fn ok(reply: &[u8], what: &str) {
    assert!(reply.starts_with(b"+OK"), "{what}: {reply:?}");
}

/// Parse a `:N` integer reply.
pub fn int_of(reply: &[u8]) -> i64 {
    let s = crate::common::lite::text(reply);
    s.trim_start_matches(':')
        .trim_end()
        .parse()
        .unwrap_or_else(|_| panic!("not an integer reply: {reply:?}"))
}

/// N-th bulk payload of a reply, skipping a leading integer count
/// (FT.SEARCH / XRANGE rows; same shape as search_e2e::bulk_at).
pub fn bulk_at(reply: &[u8], n: usize) -> String {
    let s = crate::common::lite::text(reply);
    let mut rest: &str = &s;
    if rest.starts_with(':') {
        rest = rest.split_once("\r\n").map(|(_, r)| r).unwrap_or("");
    }
    let mut items = 0usize;
    let mut it = rest.split("\r\n");
    while let Some(head) = it.next() {
        if let Some(len) = head.strip_prefix('$') {
            let len: usize = len.parse().unwrap_or(0);
            let body = it.next().unwrap_or("");
            if items == n {
                return String::from_utf8_lossy(&body.as_bytes()[..len.min(body.len())])
                    .into_owned();
            }
            items += 1;
        }
    }
    panic!("no bulk #{n} in {reply:?}");
}

/// One-shot reply as the FIRST bulk payload ("" for nil): GET/HGET/
/// JSON.GET/ZSCORE-style single-value commands.
pub async fn rb(addr: &str, args: &[&[u8]]) -> String {
    let reply = r(addr, args).await;
    if reply.starts_with(b"$-1") || reply.starts_with(b"*-1") {
        return String::new();
    }
    bulk_at(&reply, 0)
}

/// Poll a one-shot command until its reply contains `needle`.
pub async fn until_contains(addr: &str, args: &[&[u8]], needle: &str, what: &str, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let got = r(addr, args).await;
        let s = crate::common::lite::text(&got);
        if s.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: reply never contained {needle:?}, last: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Every stream-entry id inside an XRANGE/XREADGROUP full reply: ids
/// are the bulk payloads matching `<digits>-<digits>` with an exact
/// length header (delay exchange mints FRESH ids, so callers must read
/// ids out of the reply, not the XADD receipt).
pub fn stream_ids(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut ids = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        if let Some(len) = toks[i].strip_prefix('$') {
            let body = toks[i + 1];
            if let Ok(n) = len.parse::<usize>() {
                if n == body.len()
                    && body.split_once('-').is_some_and(|(a, b)| {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.bytes().all(|c| c.is_ascii_digit())
                            && b.bytes().all(|c| c.is_ascii_digit())
                    })
                {
                    ids.push(body.to_string());
                }
            }
        }
        i += 1;
    }
    ids
}

// ---- HTTP shims (ES front + RocksMQ HTTP front) --------------------

/// One single-shot HTTP round trip (`Connection: close`): `(status,
/// body)`. Connect failures surface as status 0 for poll loops.
pub async fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: e2e\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
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

/// RocksMQ HTTP POST (produce/consume/ack target with query string).
pub async fn mq_post(addr: &str, target: &str, body: &str) -> (u16, String) {
    http(addr, "POST", target, Some(body)).await
}

/// Parse a JSON ES/RocksMQ body (panics naming the body on error).
pub fn json_of(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("bad json {e}: {s}"))
}

// ---- S3 shims (bearer-token raw client, one request per conn) ------

/// One S3 round trip with the fixture bearer token: `(status, head,
/// body)`.
pub async fn s3_call(
    addr: &str,
    method: &str,
    target: &str,
    body: &[u8],
) -> (u16, String, Vec<u8>) {
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: s3-e2e\r\nContent-Length: {}\r\n\
         Authorization: Bearer {S3_TOKEN}\r\n\r\n",
        body.len()
    );
    let mut sock = TcpStream::connect(addr).await.expect("connect s3");
    sock.write_all(req.as_bytes()).await.expect("write s3 head");
    sock.write_all(body).await.expect("write s3 body");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read s3 reply");
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("s3 head \\r\\n\\r\\n");
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head, buf[head_end + 4..].to_vec())
}
