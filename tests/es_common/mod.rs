//! Shared helpers for the ES-front process-level e2e tests
//! (`tests/es_e2e.rs`, `tests/es_search_e2e.rs`): raw HTTP/1.1 round
//! trips against the spawned node's `es_bind` port and the spawn +
//! readiness loop. The v1 transport is one request per connection
//! (`Connection: close`), so every call is a fresh TcpStream read to
//! EOF; connect/read failures surface as status 0 so poll loops can
//! simply retry.

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// Mounted as a submodule of a test binary that also declares
// `mod common;`, so the shared ProcNode type stays ONE type.
use crate::common::{spawn_node_es, wait_resp_ready, ProcNode};

/// One HTTP round trip: `(status, body)`; `(0, "")` on connect/read
/// failure or timeout (startup polling).
pub async fn http(addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: e2e\r\nContent-Length: {}\r\n\r\n{body}",
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

pub fn json_body(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("bad json body {s:?}: {e}"))
}

/// Spawn an ES-enabled node (fresh ports, es_bind appended to the
/// base yaml) and wait until GET / answers 200. `wait_resp_ready`
/// first also fails fast on an early child exit with its stderr tail.
pub async fn spawn_es_ready(tag: &str) -> (ProcNode, String) {
    let dir = std::env::temp_dir().join(format!("rdb-es-e2e-{tag}-{}", std::process::id()));
    let mut node = spawn_node_es(&dir, 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    let es = node.es.clone();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, _) = http(&es, "GET", "/", None).await;
        if status == 200 {
            return (node, es);
        }
        assert!(
            Instant::now() < deadline,
            "es port never came up; {}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
