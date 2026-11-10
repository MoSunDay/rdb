//! Process-level e2e for the RocksMQ HTTP front's WP4 surface:
//! `wait_ms` long-poll consume (timeout + plain/delayed wake),
//! `POST /pending` summary shape, `delay_ms` produce passthrough and
//! the `rocksmq_token` Bearer matrix. Same shape as
//! `rocksmq_http_e2e.rs` (one REAL rdb binary per test, raw TCP);
//! requests are single-shot `Connection: close` (one read-to-EOF =
//! one full response); the delay sweep is armed via
//! `lite.delay_sweep_ms` in the node yaml.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// FAKE raft token every node yaml carries (never a real secret).
const TOKEN: &str = "e2e-fake-token-0123456789abcdef0123456789abcdef";
/// FAKE rocksmq bearer token for the gated node (never a real secret).
const MQ_TOKEN: &str = "e2e-fake-mq-token-0123456789abcdef";
/// Delayed-message sweep rhythm armed in every node yaml (production
/// default is 0 = off; the delay tests need it running).
const SWEEP_MS: u64 = 100;
const B64_WAKE: &str = "d2FrZS11cA=="; // base64 "wake-up"
const B64_LATER: &str = "bGF0ZXI="; // base64 "later"
const B64_DL: &str = "ZGw="; // base64 "dl"

// ---- fixture -------------------------------------------------------------

struct Node {
    child: Child,
    dir: PathBuf,
    stderr_path: PathBuf,
    http: String,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawn one node; `mq_token` empty = gate off (the default posture).
fn spawn_node(dir: &str, mq_token: &str) -> Node {
    std::fs::create_dir_all(dir).expect("create node dir");
    let a: Vec<String> = (0..5)
        .map(|_| {
            TcpListener::bind("127.0.0.1:0")
                .expect("bind ephemeral")
                .local_addr()
                .expect("local_addr")
                .to_string()
        })
        .collect();
    let gate = (mq_token.is_empty())
        .then(String::new)
        .unwrap_or_else(|| format!("rocksmq_token: \"{mq_token}\"\n"));
    let yaml = format!(
        "bind: \"{}\"\nstore_path: \"{dir}\"\nraft_bind_address: \"{}\"\n\
         raft_http_bind_address: \"{}\"\nmonitor_addr: \"{}\"\n\
         raft_token: \"{TOKEN}\"\nrocksmq_bind: \"{}\"\n{gate}\
         lite:\n  delay_sweep_ms: {SWEEP_MS}\n",
        a[0], a[1], a[2], a[3], a[4]
    );
    let config_path = PathBuf::from(dir).join("conf.yaml");
    std::fs::write(&config_path, yaml).expect("write conf.yaml");
    let stderr_path = PathBuf::from(dir).join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
    let child = Command::new(env!("CARGO_BIN_EXE_rdb"))
        .arg("-config")
        .arg(&config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn rdb binary");
    Node {
        child,
        dir: PathBuf::from(dir),
        stderr_path,
        http: a[4].clone(),
    }
}

/// Boot a node and wait for its HTTP listener to accept.
async fn node_up(tag: &str, mq_token: &str) -> Node {
    let mut n = spawn_node(&dir_for(tag), mq_token);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = n.child.try_wait() {
            let tail = std::fs::read_to_string(&n.stderr_path).unwrap_or_default();
            panic!("rdb exited before http ready (status {status})\nstderr:\n{tail}");
        }
        if TcpStream::connect(&n.http).await.is_ok() {
            return n;
        }
        assert!(Instant::now() < deadline, "http not accepting in 15s");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- HTTP client ---------------------------------------------------------

/// One single-shot request (fresh connection, `Connection: close`):
/// the read-to-EOF is one full response; `(status, head, body)`.
async fn post(
    addr: &str,
    verb: &str,
    target: &str,
    body: &str,
    auth: Option<&str>,
) -> (u16, String, String) {
    let auth_line = auth
        .map(|v| format!("Authorization: {v}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{verb} {target} HTTP/1.1\r\nHost: e2e\r\n{auth_line}\
         Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = TcpStream::connect(addr).await.expect("connect http");
    sock.write_all(req.as_bytes()).await.expect("write http");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read http");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, out) = text.split_once("\r\n\r\n").expect("head+body");
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, head.into(), out.into())
}

/// A parked `/consume?...&wait_ms=...` request as a background task
/// returning `(status, body, elapsed-since-issue)` -- the waiter.
fn parked(addr: &str, target: &str) -> tokio::task::JoinHandle<(u16, String, Duration)> {
    let (addr, target) = (addr.to_string(), target.to_string());
    tokio::spawn(async move {
        let t0 = Instant::now();
        let (s, _, b) = post(&addr, "POST", &target, "", None).await;
        (s, b, t0.elapsed())
    })
}

fn msgs_of(body: &str) -> Vec<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(body).expect("consume json");
    v["msgs"]
        .as_array()
        .expect("msgs array")
        .iter()
        .map(|m| {
            (
                m["id"].as_str().expect("id").to_string(),
                m["body"].as_str().expect("body").to_string(),
            )
        })
        .collect()
}

/// POST /pending and parse its JSON body (panics on a non-200).
async fn pend_json(addr: &str, target: &str) -> serde_json::Value {
    let (s, _, b) = post(addr, "POST", target, "", None).await;
    assert_eq!(s, 200, "{b}");
    serde_json::from_str(&b).expect("pending json")
}

fn dir_for(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("rdb-rocksmq-wp4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.display().to_string()
}

// ---- tests ---------------------------------------------------------------

#[tokio::test]
async fn wait_ms_times_out_empty_and_keeps_defaults() {
    let node = node_up("wait-timeout", "").await;
    // Default and wait_ms=0: a dry group consume answers empty AT ONCE.
    for q in [
        "/consume?channel=w0&group=g",
        "/consume?channel=w0&group=g&wait_ms=0",
    ] {
        let t0 = Instant::now();
        let (s, _, b) = post(&node.http, "POST", q, "", None).await;
        assert_eq!((s, msgs_of(&b).len()), (200, 0), "{b}");
        assert!(t0.elapsed() < Duration::from_millis(2000), "immediate");
    }
    // wait_ms=500 on an empty channel: parks the full budget, then answers the same graceful empty reply.
    let t0 = Instant::now();
    let q = "/consume?channel=w0&group=g&wait_ms=500";
    let (s, _, b) = post(&node.http, "POST", q, "", None).await;
    assert_eq!((s, b.as_str()), (200, r#"{"msgs":[]}"#), "{b}");
    let el = t0.elapsed();
    assert!(
        Duration::from_millis(480) <= el && el < Duration::from_secs(5),
        "{el:?}"
    );
    // Non-numeric / negative wait_ms -> 400 (the `n` error family).
    for bad in ["wait_ms=x", "wait_ms=-1"] {
        let q = format!("/consume?channel=w0&group=g&{bad}");
        let (s, _, b) = post(&node.http, "POST", &q, "", None).await;
        assert_eq!(s, 400, "{bad}: {b}");
    }
}

#[tokio::test]
async fn wait_wake_and_delayed_visibility() {
    let node = node_up("wake-delay", "").await;

    // A plain produce wakes a parked group consumer with the entry.
    let waiter = parked(&node.http, "/consume?channel=wk&group=g&wait_ms=15000");
    tokio::time::sleep(Duration::from_millis(400)).await; // let it park
    let (s, _, id) = post(&node.http, "POST", "/produce?channel=wk", "wake-up", None).await;
    assert_eq!(s, 200, "{id}");
    let (s, b, el) = tokio::time::timeout(Duration::from_secs(25), waiter)
        .await
        .expect("waiter resolves")
        .expect("join ok");
    let msgs = msgs_of(&b);
    assert_eq!(
        (s, msgs.len(), msgs[0].1.as_str()),
        (200, 1, B64_WAKE),
        "{b}"
    );
    assert!(el < Duration::from_secs(5), "woken, not timed out");

    // delay_ms=800: NOT consumable before due (a staged row is
    // invisible), consumable after the sweep exchanges it.
    let (s, _, id) = post(
        &node.http,
        "POST",
        "/produce?channel=dch&delay_ms=800",
        "dl",
        None,
    )
    .await;
    assert_eq!(s, 200, "{id}");
    let q = "/consume?channel=dch&group=dg&wait_ms=200";
    let (s, _, b) = post(&node.http, "POST", q, "", None).await;
    assert_eq!((s, b.as_str()), (200, r#"{"msgs":[]}"#), "pre-due");
    let deadline = Instant::now() + Duration::from_secs(15);
    let got = loop {
        assert!(Instant::now() < deadline, "delayed entry never landed");
        let (s, _, b) = post(
            &node.http,
            "POST",
            "/consume?channel=dch&group=dg",
            "",
            None,
        )
        .await;
        assert_eq!(s, 200, "{b}");
        let m = msgs_of(&b);
        if !m.is_empty() {
            break m;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(got[0].1, B64_DL, "post-due body");
    assert_ne!(got[0].0, id, "fresh id at exchange");

    // A delayed produce wakes a parked reader only AT the exchange:
    // entry arrives after the 600ms due, well before the 15s budget.
    let waiter = parked(&node.http, "/consume?channel=wd&group=g&wait_ms=15000");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let q = "/produce?channel=wd&delay_ms=600";
    let (s, _, reserved) = post(&node.http, "POST", q, "later", None).await;
    assert_eq!(s, 200, "{reserved}");
    let (s, b, el) = tokio::time::timeout(Duration::from_secs(25), waiter)
        .await
        .expect("waiter resolves")
        .expect("join ok");
    let msgs = msgs_of(&b);
    assert_eq!(
        (s, msgs.len(), msgs[0].1.as_str()),
        (200, 1, B64_LATER),
        "{b}"
    );
    assert!(
        el >= Duration::from_millis(600) && el < Duration::from_secs(8),
        "{el:?}"
    );
    assert_ne!(msgs[0].0, reserved, "fresh id at exchange");

    // delay_ms=0 is a plain produce: immediately consumable. Invalid
    // values are 400; the absent form keeps today's argv (rocksmq_http_e2e).
    let q = "/produce?channel=d0&delay_ms=0";
    let (s, _, _) = post(&node.http, "POST", q, "now", None).await;
    assert_eq!(s, 200);
    let (s, _, b) = post(&node.http, "POST", "/consume?channel=d0&group=dg", "", None).await;
    assert_eq!((s, msgs_of(&b).len()), (200, 1), "{b}");
    for bad in ["delay_ms=x", "delay_ms=-5"] {
        let q = format!("/produce?channel=dch&{bad}");
        let (s, _, b) = post(&node.http, "POST", &q, "m", None).await;
        assert_eq!(s, 400, "{bad}: {b}");
    }
}

#[tokio::test]
async fn pending_summary_and_error_face() {
    let node = node_up("pending", "").await;
    // produce 3, group-consume all 3 without ack -> the PEL holds 3.
    let mut ids = Vec::new();
    for m in ["p1", "p2", "p3"] {
        let (s, _, id) = post(&node.http, "POST", "/produce?channel=pch", m, None).await;
        assert_eq!(s, 200, "{id}");
        ids.push(id);
    }
    let q = "/consume?channel=pch&group=pg&n=3";
    let (s, _, b) = post(&node.http, "POST", q, "", None).await;
    assert_eq!((s, msgs_of(&b).len()), (200, 3), "{b}");

    // /pending mirrors the XPENDING summary of the same PEL: min/max
    // are the first/last delivery ids; one logical consumer "http".
    let v = pend_json(&node.http, "/pending?channel=pch&group=pg").await;
    assert_eq!(v["channel"], "pch", "{v}");
    assert_eq!(v["group"], "pg", "{v}");
    assert_eq!(v["pending"], 3, "{v}");
    assert_eq!(v["min_id"], ids[0].as_str(), "{v}");
    assert_eq!(v["max_id"], ids[2].as_str(), "{v}");
    assert_eq!(
        v["consumers"],
        serde_json::json!([{"name": "http", "pending": 3}]),
        "{v}"
    );

    // ack one, then all: pending drops to 2 (min advances), and the
    // empty PEL renders 0 / null / null / [].
    let q = format!("/ack?channel=pch&group=pg&id={}", ids[0]);
    let (s, _, r) = post(&node.http, "POST", &q, "", None).await;
    assert_eq!((s, r.as_str()), (200, "ok"), "{r}");
    let v = pend_json(&node.http, "/pending?channel=pch&group=pg").await;
    assert_eq!(v["pending"], 2, "{v}");
    assert_eq!(v["min_id"], ids[1].as_str(), "{v}");
    assert_eq!(v["max_id"], ids[2].as_str(), "{v}");
    for id in [&ids[1], &ids[2]] {
        let q = format!("/ack?channel=pch&group=pg&id={id}");
        let (s, _, _) = post(&node.http, "POST", &q, "", None).await;
        assert_eq!(s, 200);
    }
    let v = pend_json(&node.http, "/pending?channel=pch&group=pg").await;
    assert_eq!(v["pending"], 0, "{v}");
    assert!(v["min_id"].is_null() && v["max_id"].is_null(), "{v}");
    assert_eq!(v["consumers"], serde_json::json!([]), "{v}");

    // Error face: missing param 400, unknown group 404, GET 405.
    let (s, _, _) = post(&node.http, "POST", "/pending?channel=pch", "", None).await;
    assert_eq!(s, 400, "missing group");
    let (s, _, _) = post(
        &node.http,
        "POST",
        "/pending?channel=pch&group=never",
        "",
        None,
    )
    .await;
    assert_eq!(s, 404, "unknown group");
    let q = "/pending?channel=pch&group=pg";
    let (s, head, _) = post(&node.http, "GET", q, "", None).await;
    assert!(
        s == 405 && head.contains("Allow: POST"),
        "GET /pending: {head}"
    );
}

#[tokio::test]
async fn bearer_token_matrix() {
    // Gate OFF (default): every route answers with no Authorization.
    let open = node_up("auth-open", "").await;
    for (target, body) in [
        ("/produce?channel=au", "m"),
        ("/consume?channel=au&group=g", ""),
        ("/pending?channel=au&group=g", ""),
        ("/ack?channel=au&group=g&id=1-1", ""),
    ] {
        let (s, _, b) = post(&open.http, "POST", target, body, None).await;
        assert_eq!(s, 200, "open {target}: {b}");
    }
    // Gate ON (FAKE token): 401 for missing/wrong credentials; fixed body, no echo.
    let gated = node_up("auth-gated", MQ_TOKEN).await;
    for auth in [None, Some("Bearer wrong-token"), Some("Basic xyz")] {
        let (s, head, b) = post(&gated.http, "POST", "/produce?channel=au", "", auth).await;
        assert_eq!(s, 401, "{auth:?}: {head}");
        assert_eq!(b, "unauthorized", "fixed body: {b}");
        assert!(!head.contains(MQ_TOKEN) && !b.contains(MQ_TOKEN), "no echo");
    }
    // Every route is gated; the correct bearer drives the whole
    // produce -> consume -> ack -> pending flow through.
    let bearer = format!("Bearer {MQ_TOKEN}");
    for (target, body) in [
        ("/produce?channel=au", "m"),
        ("/consume?channel=au&group=g", ""),
        ("/ack?channel=au&group=g&id=1-1", ""),
        ("/pending?channel=au&group=g", ""),
    ] {
        let (s, _, _) = post(&gated.http, "POST", target, "", None).await;
        assert_eq!(s, 401, "gated {target}");
        let (s, _, b) = post(&gated.http, "POST", target, body, Some(&bearer)).await;
        assert_eq!(s, 200, "bearer passes {target}: {b}");
    }
}
