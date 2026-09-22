//! RocksMQ-style HTTP API, process-level e2e: one REAL rdb binary with
//! `rocksmq_bind` set, driven over raw TCP HTTP/1.1 (hand-rolled
//! requests, like every other front's e2e) plus the RESP port for the
//! interop proofs (XADD/XACK/XPENDING/XRANGE on the same streams).

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TOKEN: &str = "e2e-fake-token-0123456789abcdef0123456789abcdef";

// ---- fixture -------------------------------------------------------------

struct Node {
    child: Child,
    dir: PathBuf,
    #[allow(dead_code)] // kept for symmetry with the other e2e fixtures
    config_path: PathBuf,
    stderr_path: PathBuf,
    resp: String,
    http: String,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn free_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    l.local_addr().expect("local_addr").to_string()
}

fn spawn_rocksmq_node(dir: &str) -> Node {
    std::fs::create_dir_all(dir).expect("create node dir");
    let (resp, raft, raft_http, monitor, http) =
        (free_addr(), free_addr(), free_addr(), free_addr(), free_addr());
    let config_path = PathBuf::from(dir).join("conf.yaml");
    let yaml = format!(
        "bind: \"{resp}\"\nstore_path: \"{dir}\"\nraft_bind_address: \"{raft}\"\n\
         raft_http_bind_address: \"{raft_http}\"\nmonitor_addr: \"{monitor}\"\n\
         raft_token: \"{TOKEN}\"\nrocksmq_bind: \"{http}\"\n",
    );
    std::fs::write(&config_path, yaml).expect("write conf.yaml");
    let stderr_path = PathBuf::from(dir).join("stderr.log");
    let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
    let child = Command::new(env!("CARGO_BIN_EXE_rdb"))
        .arg("-config")
        .arg(&config_path)
        .env("RAFT_BOOTSTRAP", "true")
        .env_remove("RAFT_JOIN_ADDR")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn rdb binary");
    Node {
        child,
        dir: PathBuf::from(dir),
        config_path,
        stderr_path,
        resp,
        http,
    }
}

async fn wait_accepting(node: &mut Node, addr: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = node.child.try_wait() {
            let tail = std::fs::read_to_string(&node.stderr_path).unwrap_or_default();
            panic!("rdb exited before {what} ready (status {status})\nstderr:\n{tail}");
        }
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} {addr} not accepting in 15s\nstderr:\n{}",
            std::fs::read_to_string(&node.stderr_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ---- HTTP client ---------------------------------------------------------

/// POST helper: fresh connection, one round.
async fn post(addr: &str, target: &str, body: &str) -> (u16, String) {
    let mut sock = TcpStream::connect(addr).await.expect("connect http");
    let req = format!(
        "POST {target} HTTP/1.1\r\nHost: e2e\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, head, out_body) = round_trip(&mut sock, &req).await;
    let _ = head;
    (status, out_body)
}

/// Read one full response (head + content-length body) off `sock`.
async fn round_trip(sock: &mut TcpStream, req: &str) -> (u16, String, String) {
    sock.write_all(req.as_bytes()).await.expect("write http req");
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.expect("read http head");
        assert!(n > 0, "eof mid-head");
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let len: usize = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
        .expect("content-length");
    while buf.len() < head_end + 4 + len {
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.expect("read http body");
        assert!(n > 0, "eof mid-body");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end + 4..head_end + 4 + len]).to_string();
    (status, head, body)
}

/// AUTH + one RESP command on a fresh connection; returns the command
/// reply (AUTH's +OK stripped).
async fn resp_cmd(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut sock = TcpStream::connect(addr).await.expect("connect resp");
    let mut buf = b"*2\r\n$4\r\nAUTH\r\n".to_vec();
    buf.extend_from_slice(format!("${}\r\n{}\r\n", TOKEN.len(), TOKEN).as_bytes());
    buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    sock.write_all(&buf).await.expect("write resp cmds");
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_millis(300), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
        }
    }
    out.split_at(5).1.to_vec() // drop "+OK\r\n"
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

fn dir_for(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("rdb-rocksmq-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.display().to_string()
}

// ---- tests ---------------------------------------------------------------

#[tokio::test]
async fn produce_group_consume_ack_flow() {
    let dir = dir_for("flow");
    let mut node = spawn_rocksmq_node(&dir);
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "rocksmq http").await;

    // 1. produce 3 messages; ids look like <ms>-<seq>.
    let mut ids = Vec::new();
    for m in ["hello", "m2", "m3"] {
        let (status, id) = post(&node.http, "/produce?channel=ch1", m).await;
        assert_eq!(status, 200, "produce {m}: {id}");
        assert!(
            id.contains('-') && id.split('-').all(|p| p.parse::<u64>().is_ok()),
            "id shape: {id}"
        );
        ids.push(id);
    }
    assert!(ids[0] < ids[1] && ids[1] < ids[2], "ascending ids: {ids:?}");

    // 2. group consume n=2 -> first two in order; then 1; then empty.
    let (s, b) = post(&node.http, "/consume?channel=ch1&group=g1&n=2", "").await;
    assert_eq!(s, 200, "{b}");
    let got = msgs_of(&b);
    assert_eq!(got.len(), 2, "{b}");
    assert_eq!(
        (got[0].0.as_str(), got[0].1.as_str()),
        (ids[0].as_str(), "aGVsbG8="),
        "first entry: {b}"
    );
    assert_eq!(got[1].0.as_str(), ids[1].as_str());
    let (s, b) = post(&node.http, "/consume?channel=ch1&group=g1&n=2", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 1), "{b}");
    assert_eq!(msgs_of(&b)[0].0, ids[2]);
    let (s, b) = post(&node.http, "/consume?channel=ch1&group=g1&n=2", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 0), "drained: {b}");

    // 3. ack: HTTP idempotent + RESP XACK interop + empty PEL proof.
    let (s, b) = post(
        &node.http,
        &format!("/ack?channel=ch1&group=g1&id={}", ids[0]),
        "",
    )
    .await;
    assert_eq!((s, b.as_str()), (200, "ok"), "first ack");
    let (s, b) = post(
        &node.http,
        &format!("/ack?channel=ch1&group=g1&id={}", ids[0]),
        "",
    )
    .await;
    assert_eq!((s, b.as_str()), (200, "ok"), "re-ack is idempotent");
    let resp = resp_cmd(&node.resp, &[b"XACK", b"ch1/q0", b"g1", ids[1].as_bytes()]).await;
    assert_eq!(resp, b":1\r\n".to_vec(), "RESP XACK of the second id");
    // The third delivery is still pending; ack it over HTTP, then the
    // whole PEL must be empty (XPENDING summary total = 0).
    let (s, b) = post(
        &node.http,
        &format!("/ack?channel=ch1&group=g1&id={}", ids[2]),
        "",
    )
    .await;
    assert_eq!((s, b.as_str()), (200, "ok"), "ack of the last delivery");
    let pend = resp_cmd(&node.resp, &[b"XPENDING", b"ch1/q0", b"g1"]).await;
    assert!(
        pend.windows(4).any(|w| w == b"*3\r\n") && pend.windows(4).any(|w| w == b":0\r\n"),
        "PEL must be empty: {:?}",
        String::from_utf8_lossy(&pend)
    );
    // unknown group -> 404
    let (s, _) = post(&node.http, "/ack?channel=ch1&group=nope&id=1-1", "").await;
    assert_eq!(s, 404, "ack of unknown group");
}

#[tokio::test]
async fn interop_resp_xadd_and_back() {
    let dir = dir_for("interop");
    let mut node = spawn_rocksmq_node(&dir);
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "rocksmq http").await;
    let resp_addr = node.resp.clone();
    wait_accepting(&mut node, &resp_addr, "resp").await;

    // RESP XADD lands in ch2/q0; a NEW HTTP group consume sees it
    // (auto-created group starts at the stream head).
    let resp = resp_cmd(&node.resp, &[b"XADD", b"ch2/q0", b"*", b"v", b"world"]).await;
    assert!(resp.starts_with(b"$"), "xadd id: {:?}", String::from_utf8_lossy(&resp));
    let (s, b) = post(&node.http, "/consume?channel=ch2&group=g9", "").await;
    assert_eq!(s, 200, "{b}");
    let got = msgs_of(&b);
    assert_eq!(got.len(), 1, "{b}");
    assert_eq!(got[0].1, "d29ybGQ=", "base64(world): {b}");

    // HTTP produce lands in the SAME stream (XRANGE sees both entries
    // with the 'v' field) -- the shared 1-pair rule with the Kafka front.
    let (s, _) = post(&node.http, "/produce?channel=ch2", "http-msg").await;
    assert_eq!(s, 200);
    let range = resp_cmd(&node.resp, &[b"XRANGE", b"ch2/q0", b"-", b"+"]).await;
    let text = String::from_utf8_lossy(&range).to_string();
    assert!(text.contains("*2\r\n"), "two entries: {text}");
    assert!(text.contains("$1\r\nv\r\n$5\r\nworld") && text.contains("http-msg"), "fields: {text}");

    // Bare channel name == explicit parent/child (both hit q0).
    let (_, b1) = post(&node.http, "/consume?channel=ch2&n=10", "").await;
    let (_, b2) = post(&node.http, "/consume?channel=ch2%2Fq0&n=10", "").await;
    assert_eq!(msgs_of(&b1).len(), 2, "{b1}");
    assert_eq!(b1, b2, "bare and full names address the same stream");
}

#[tokio::test]
async fn no_group_pull_and_error_paths() {
    let dir = dir_for("errors");
    let mut node = spawn_rocksmq_node(&dir);
    let http = node.http.clone();
    wait_accepting(&mut node, &http, "rocksmq http").await;

    // No-group consume: latest n entries, no progress (same tail again).
    for m in ["a", "b", "c"] {
        let (s, _) = post(&node.http, "/produce?channel=ch3", m).await;
        assert_eq!(s, 200);
    }
    let (s, b) = post(&node.http, "/consume?channel=ch3&n=2", "").await;
    assert_eq!(s, 200, "{b}");
    let got = msgs_of(&b);
    assert_eq!(
        (got[0].1.as_str(), got[1].1.as_str()),
        ("Yg==", "Yw=="),
        "tail two, ascending: {b}"
    );
    let (_, b2) = post(&node.http, "/consume?channel=ch3&n=2", "").await;
    assert_eq!(b, b2, "no progress recorded without a group");

    // Unknown stream: empty array, nothing created.
    let (s, b) = post(&node.http, "/consume?channel=nosuch&group=g", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 0), "{b}");
    let (s, b) = post(&node.http, "/consume?channel=nosuch&n=5", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 0), "{b}");

    // 400s: missing/empty channel, empty body, bad channel name, bad n.
    for (target, body) in [
        ("/produce", "x"),
        ("/produce?channel=", "x"),
        ("/produce?channel=bad+name", "x"),
        ("/produce?channel=ch3", ""),
        ("/consume?channel=ch3&n=0", ""),
        ("/consume?channel=ch3&n=101", ""),
        ("/consume?channel=ch3&n=x", ""),
        ("/ack?channel=ch3&group=g", ""),
        ("/produce?channel=a%2Fb%2Fc", "x"),
    ] {
        let (s, b) = post(&node.http, target, body).await;
        assert_eq!(s, 400, "{target} should be 400: {b}");
    }

    // 404 unknown path; 405 non-POST on known paths; 413 oversized body.
    let (s, _) = post(&node.http, "/nope?x=1", "").await;
    assert_eq!(s, 404);
    let mut sock = TcpStream::connect(&node.http).await.expect("connect");
    let (s, head, _) = round_trip(
        &mut sock,
        "GET /produce?channel=ch3 HTTP/1.1\r\nHost: e2e\r\n\r\n",
    )
    .await;
    assert_eq!(s, 405, "{head}");
    assert!(head.to_ascii_lowercase().contains("allow: post"), "{head}");
    // 413: the server answers and closes WITHOUT draining an oversized
    // body (the standard posture), so the client write may hit EPIPE --
    // tolerate it and just read whatever reply arrives.
    let big = "x".repeat((4 << 20) + 1);
    let mut sock = TcpStream::connect(&node.http).await.expect("connect");
    let req = format!(
        "POST /produce?channel=ch3 HTTP/1.1\r\nHost: e2e\r\nContent-Length: {}\r\n\r\n{}",
        big.len(),
        big
    );
    let _ = sock.write_all(req.as_bytes()).await; // EPIPE is expected
    let mut got = Vec::new();
    let mut chunk = [0u8; 1024];
    while got.windows(4).position(|w| w == b"\r\n\r\n").is_none() {
        match tokio::time::timeout(Duration::from_millis(500), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => got.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&got).to_string();
    assert!(head.starts_with("HTTP/1.1 413"), "oversized body reply: {head}");

    // Keep-alive: two rounds on ONE socket; Connection: close honored.
    let mut sock = TcpStream::connect(&node.http).await.expect("connect");
    for m in ["k1", "k2"] {
        let req = format!(
            "POST /produce?channel=ka HTTP/1.1\r\nHost: e2e\r\nContent-Length: {}\r\n\r\n{m}",
            m.len()
        );
        let (s, head, id) = round_trip(&mut sock, &req).await;
        assert_eq!(s, 200, "{head}");
        assert!(head.to_ascii_lowercase().contains("connection: keep-alive"), "{head}");
        assert!(id.contains('-'), "{id}");
    }
    let (s, head, _) = round_trip(
        &mut sock,
        "POST /produce?channel=ka HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\nContent-Length: 2\r\n\r\nk3",
    )
    .await;
    assert_eq!(s, 200, "{head}");
    assert!(head.to_ascii_lowercase().contains("connection: close"), "{head}");
    // the server must now have closed the socket: next read is EOF
    let mut chunk = [0u8; 16];
    let n = sock.read(&mut chunk).await.expect("read after close");
    assert_eq!(n, 0, "connection closed after Connection: close");
}
