//! Shared process-level fixture + HTTP client for the RocksMQ-front e2e
//! suites (P3 `rocksmq_batch_range_e2e.rs` and successors): boot the
//! REAL `rdb` binary (`CARGO_BIN_EXE_rdb`) with `rocksmq_bind` wired
//! plus whatever extra yaml a suite needs (`rocksmq_token`,
//! `rocksmq_max_connections`, `lite.delay_sweep_ms`), wait for the
//! HTTP listener, then speak single-shot HTTP/1.1 over raw TcpStreams.
//! The bootstrap mirrors `tests/rocksmq_wait_pending_e2e.rs`'s
//! fixture; the existing per-file fixtures are deliberately NOT
//! touched -- this module is the factored home for new files.

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// FAKE raft token every node yaml carries (never a real secret).
pub const TOKEN: &str = "e2e-fake-token-0123456789abcdef0123456789abcdef";

// ---- fixture -------------------------------------------------------------

/// One running rdb process with the rocksmq HTTP front wired.
pub struct MqNode {
    pub child: Child,
    pub dir: PathBuf,
    #[allow(dead_code)] // kept for symmetry with the per-file fixtures
    pub config_path: PathBuf,
    pub stderr_path: PathBuf,
    pub resp: String,
    pub http: String,
}

impl Drop for MqNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Spawn a node whose yaml = the standard single-node skeleton + the
/// `rocksmq_bind` line + `extra` (raw yaml lines, e.g.
/// `rocksmq_token: "..."` / `rocksmq_max_connections: 1`).
pub fn spawn_mq_node(dir: &str, extra: &str) -> MqNode {
    std::fs::create_dir_all(dir).expect("create node dir");
    let (resp, raft, raft_http, monitor, http) = (
        free_addr(),
        free_addr(),
        free_addr(),
        free_addr(),
        free_addr(),
    );
    let yaml = format!(
        "bind: \"{resp}\"\nstore_path: \"{dir}\"\nraft_bind_address: \"{raft}\"\n\
         raft_http_bind_address: \"{raft_http}\"\nmonitor_addr: \"{monitor}\"\n\
         raft_token: \"{TOKEN}\"\nrocksmq_bind: \"{http}\"\n{extra}",
    );
    let config_path = PathBuf::from(dir).join("conf.yaml");
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
    MqNode {
        child,
        dir: PathBuf::from(dir),
        config_path,
        stderr_path,
        resp,
        http,
    }
}

/// Boot + wait until the HTTP listener accepts (15s ceiling; a dead
/// child fails fast with its stderr tail).
pub async fn node_up(tag: &str, extra: &str) -> MqNode {
    let mut n = spawn_mq_node(&dir_for(tag), extra);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(Some(status)) = n.child.try_wait() {
            let tail = std::fs::read_to_string(&n.stderr_path).unwrap_or_default();
            panic!("rdb exited before http ready (status {status})\nstderr:\n{tail}");
        }
        if TcpStream::connect(&n.http).await.is_ok() {
            return n;
        }
        assert!(
            Instant::now() < deadline,
            "http {} not accepting in 15s\nstderr:\n{}",
            n.http,
            std::fs::read_to_string(&n.stderr_path).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn free_addr() -> String {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    l.local_addr().expect("local_addr").to_string()
}

pub fn dir_for(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("rdb-rocksmq-mq-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.display().to_string()
}

// ---- HTTP client ---------------------------------------------------------

/// One single-shot request (fresh connection, `Connection: close`):
/// read-to-EOF is one full response. `(status, head, body)`.
pub async fn req(
    addr: &str,
    verb: &str,
    target: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (u16, String, String) {
    let mut head = format!("{verb} {target} HTTP/1.1\r\nHost: e2e\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let req = format!(
        "{head}Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut sock = TcpStream::connect(addr).await.expect("connect http");
    sock.write_all(req.as_bytes()).await.expect("write http");
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).await.expect("read http");
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (h, out) = text.split_once("\r\n\r\n").expect("head+body");
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, h.into(), out.into())
}

/// POST with no extra headers (the ungated default posture).
pub async fn post(addr: &str, target: &str, body: &str) -> (u16, String) {
    let (status, _, body) = req(addr, "POST", target, body, &[]).await;
    (status, body)
}

/// POST + `Authorization: <value>` header.
pub async fn post_auth(addr: &str, target: &str, body: &str, auth: &str) -> (u16, String) {
    let (status, _, body) = req(addr, "POST", target, body, &[("Authorization", auth)]).await;
    (status, body)
}

/// One keep-alive round on an OPEN connection: writes a plain
/// HTTP/1.1 request and reads exactly one content-length-framed reply,
/// leaving the connection open (the caller holds the slot). `None` =
/// the server closed without a reply byte (this connection lost the
/// admission race at a small cap).
pub async fn keepalive_round(sock: &mut TcpStream, target: &str) -> Option<(u16, String)> {
    let req = format!("POST {target} HTTP/1.1\r\nHost: e2e\r\nContent-Length: 0\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write http");
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let mut chunk = [0u8; 4096];
        // EOF, RST (the cap refusal closes with unread request bytes)
        // or any error = no reply this round; surface as None.
        match sock.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let status = head
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
    Some((
        status,
        String::from_utf8_lossy(&buf[head_end + 4..head_end + 4 + len]).to_string(),
    ))
}

/// Fresh connection + one request: `None` = the server closed WITHOUT
/// writing a reply byte (the connection-cap refusal posture; a normal
/// HTTP error always has a status line).
pub async fn speak_once(addr: &str, target: &str) -> Option<(u16, String)> {
    let mut sock = TcpStream::connect(addr).await.expect("connect http");
    let req = format!(
        "POST {target} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    sock.write_all(req.as_bytes()).await.expect("write http");
    let mut buf = Vec::new();
    let res = sock.read_to_end(&mut buf).await;
    if buf.is_empty() {
        return None; // refused (silently closed): not one reply byte
    }
    res.expect("read http");
    let text = String::from_utf8_lossy(&buf).into_owned();
    if !text.starts_with("HTTP/") {
        return None;
    }
    let (h, out) = text.split_once("\r\n\r\n").expect("head+body");
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    Some((status, out.into()))
}

// ---- payload helpers -----------------------------------------------------

/// Standard base64 encode (test-side twin of the front's encoder).
pub fn b64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let mut n = (c[0] as u32) << 16;
        if let Some(b) = c.get(1) {
            n |= (*b as u32) << 8;
        }
        if let Some(b) = c.get(2) {
            n |= *b as u32;
        }
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// `{"msgs":[...]}` body -> `(id, base64 body)` pairs.
pub fn msgs_of(body: &str) -> Vec<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(body).expect("msgs json");
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

/// Batch reply -> per-item `(status, body)` pairs in request order.
pub fn items_of(body: &str) -> Vec<(u64, String)> {
    let v: serde_json::Value = serde_json::from_str(body).expect("batch json");
    v.as_array()
        .expect("batch array")
        .iter()
        .map(|m| {
            (
                m["status"].as_u64().expect("item status"),
                m["body"].as_str().expect("item body").to_string(),
            )
        })
        .collect()
}

/// `/pending` count for one channel/group (`pending` field).
pub async fn pending_count(addr: &str, channel: &str, group: &str) -> u64 {
    let (status, body) = post(
        addr,
        &format!("/pending?channel={channel}&group={group}"),
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<serde_json::Value>(&body).expect("pending json")["pending"]
        .as_u64()
        .expect("pending count")
}
