//! Process-level /metrics endpoint test: spawn the REAL rdb binary,
//! drive a few RESP commands (so the latency histogram gains labeled
//! series), then scrape the Prometheus endpoint over raw HTTP on the
//! monitor port the harness wrote into the config. Asserts the 200 +
//! exposition body (rdb_command_latency, raft_stats -- and the
//! zero-initialized lite gauges) plus the plain 404 for non-/metrics
//! paths. The raft_stats gauge is only written by the 5s stats ticker,
//! so the scrape is polled, not fired once.

mod common;

use std::time::{Duration, Instant};

use common::{cmd_one_shot, contains_bytes, spawn_node, wait_resp_ready, TOKEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One raw HTTP/1.1 GET: write the request head, read to EOF (the
/// monitor closes the connection after the response).
async fn http_get(addr: &str, target: &str) -> Vec<u8> {
    let mut sock = match tokio::net::TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => return format!("<CONN-ERR {e}>").into_bytes(),
    };
    let req = format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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

/// Poll GET /metrics until the body carries both metric families (the
/// raft_stats gauge appears with the first 5s ticker run).
async fn wait_metrics(node: &common::ProcNode, secs: u64) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let resp = http_get(&node.monitor, "/metrics").await;
        let ok = contains_bytes(&resp, b"HTTP/1.1 200 OK")
            && contains_bytes(&resp, b"rdb_command_latency_bucket")
            && contains_bytes(&resp, b"raft_stats{status=");
        if ok {
            return resp;
        }
        assert!(
            Instant::now() < deadline,
            "metrics scrape never carried both families\nlast={:?}\n{}",
            String::from_utf8_lossy(&resp),
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_endpoint_serves_prometheus_families_and_404() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;

    // A couple of real commands: the resp layer observes one labeled
    // latency sample per command, so the histogram family exists.
    let r = node.resp.clone();
    assert_eq!(
        cmd_one_shot(&r, TOKEN, &[b"set", b"mkey", b"mval"]).await,
        b"+OK",
        "set mkey\n{}",
        node.ctx()
    );
    assert_eq!(
        cmd_one_shot(&r, TOKEN, &[b"get", b"mkey"]).await,
        b"$4\r\nmval",
        "get mkey\n{}",
        node.ctx()
    );

    // /metrics: 200 + the Go-parity families. The latency sample for the
    // `set` above lands as a labeled series (type/mode/ack labels).
    let body = wait_metrics(&node, 30).await;
    assert!(
        contains_bytes(&body, b"rdb_command_latency_bucket"),
        "latency histogram missing: {}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        contains_bytes(&body, &b"raft_stats{status=\"Leader\"} 1"[..]),
        "a lone bootstrapped node must export Leader=1: {}",
        String::from_utf8_lossy(&body)
    );
    // Zero-initialized lite gauges are exported even in normal mode.
    assert!(
        contains_bytes(&body, b"rdb_lite_streams"),
        "lite gauges missing: {}",
        String::from_utf8_lossy(&body)
    );

    // Anything but /metrics is the plain 404 (one request per conn).
    let not_found = http_get(&node.monitor, "/definitely-not-metrics").await;
    assert!(
        contains_bytes(&not_found, b"HTTP/1.1 404 Not Found"),
        "non-metrics path must 404: {:?}",
        String::from_utf8_lossy(&not_found)
    );
    // A query string still routes to /metrics (200, not 404).
    let query = http_get(&node.monitor, "/metrics?x=1").await;
    assert!(
        contains_bytes(&query, b"HTTP/1.1 200 OK"),
        "/metrics?query must 200: {:?}",
        String::from_utf8_lossy(&query)
    );
}
