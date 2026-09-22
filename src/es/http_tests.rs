//! Runtime smoke tests for the ES transport: `serve` on an ephemeral
//! port against a tempdir Shared, raw-socket HTTP exactly as clients
//! send it (including the `Expect: 100-continue` interim response
//! curl emits for large `_bulk` bodies).

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::state::{self, Shared};

use super::{bind, serve};

/// Read one full reply off `sock` (head + Content-Length body).
async fn read_reply(sock: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let deadline = Duration::from_secs(10);
    loop {
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(deadline, sock.read(&mut chunk))
            .await
            .expect("read timeout")
            .expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let len: usize = head
                .lines()
                .filter_map(|l| l.split_once(": "))
                .find_map(|(n, v)| {
                    n.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if buf.len() >= head_end + 4 + len {
                break;
            }
        }
    }
    let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("head end");
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let status = head.split(' ').nth(1).expect("status").to_string();
    (status, buf[head_end + 4..].to_vec())
}

/// One request/reply round trip on a fresh connection (v1 is
/// Connection: close).
async fn roundtrip(addr: &str, raw: &[u8]) -> (String, Vec<u8>) {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    sock.write_all(raw).await.expect("write");
    read_reply(&mut sock).await
}

fn shared() -> Arc<Shared> {
    Arc::new(state::testutil::shared_with(state::testutil::test_config()))
}

fn has(body: &[u8], needle: &str) -> bool {
    body.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn full_surface_over_raw_sockets() {
    let shared = shared();
    let listener = bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    tokio::spawn(serve(listener, shared, "tok".to_string()));

    // auth gate: no header -> 401 envelope
    let (status, body) = roundtrip(&addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(status, "401");
    assert!(has(&body, "security_exception"), "{body:?}");

    let get = |path: &str| {
        format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\n\r\n")
    };
    // root
    let (status, body) = roundtrip(&addr, get("/").as_bytes()).await;
    assert_eq!(status, "200");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["tagline"], "You Know, for Search");
    assert_eq!(v["cluster_name"], "rdb");

    // cluster health (single-node topology -> yellow)
    let (status, body) = roundtrip(&addr, get("/_cluster/health").as_bytes()).await;
    assert_eq!(status, "200");
    assert!(has(&body, "yellow"), "{body:?}");

    // create index, then the duplicate PUT -> 409
    let mappings = r#"{"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},"n":{"type":"long"}}}}"#;
    let put = format!(
        "PUT /books HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\nContent-Length: {}\r\n\r\n{mappings}",
        mappings.len()
    );
    let (status, body) = roundtrip(&addr, put.as_bytes()).await;
    assert_eq!(status, "200", "{}", String::from_utf8_lossy(&body));
    let (status, _) = roundtrip(&addr, put.as_bytes()).await;
    assert_eq!(status, "409");

    // GET the index back (mappings round trip through the schema)
    let (status, body) = roundtrip(&addr, get("/books").as_bytes()).await;
    assert_eq!(status, "200");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["books"]["mappings"]["properties"]["title"]["type"], "text");
    assert_eq!(v["books"]["mappings"]["properties"]["n"]["type"], "long");

    // index a doc (201), re-index (200), search + count
    let doc = br#"{"title":"hello redis world","tag":"red","n":42}"#;
    let mut raw = format!(
        "POST /books/_doc/1 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\nContent-Length: {}\r\n\r\n",
        doc.len()
    )
    .into_bytes();
    raw.extend_from_slice(doc);
    let (status, body) = roundtrip(&addr, &raw).await;
    assert_eq!(status, "201");
    assert!(has(&body, "\"result\":\"created\""), "{body:?}");
    let (status, _) = roundtrip(&addr, &raw).await;
    assert_eq!(status, "200");

    let q = br#"{"query":{"match":{"title":"redis"}}}"#;
    let mut raw = format!(
        "POST /books/_search HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\nContent-Length: {}\r\n\r\n",
        q.len()
    )
    .into_bytes();
    raw.extend_from_slice(q);
    let (status, body) = roundtrip(&addr, &raw).await;
    assert_eq!(status, "200");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["hits"]["total"]["value"], 1);
    assert_eq!(v["hits"]["hits"][0]["_id"], "1");
    assert_eq!(v["hits"]["hits"][0]["_source"]["tag"], "red");

    let (status, body) = roundtrip(&addr, get("/books/_count").as_bytes()).await;
    assert_eq!(status, "200");
    assert!(has(&body, "\"count\":1"), "{body:?}");

    // _cat table lists the index
    let (status, body) = roundtrip(&addr, get("/_cat/indices").as_bytes()).await;
    assert_eq!(status, "200");
    assert!(has(&body, "books"), "{body:?}");

    // delete the doc: "deleted", then "not_found" (both 200)
    let del = "DELETE /books/_doc/1 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok\r\n\r\n";
    let (status, body) = roundtrip(&addr, del.as_bytes()).await;
    assert_eq!(status, "200");
    assert!(has(&body, "\"result\":\"deleted\""), "{body:?}");
    let (status, body) = roundtrip(&addr, del.as_bytes()).await;
    assert_eq!(status, "200");
    assert!(has(&body, "\"result\":\"not_found\""), "{body:?}");
}

#[tokio::test]
async fn bulk_with_100_continue() {
    let shared = shared();
    let listener = bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    tokio::spawn(serve(listener, shared, String::new()));

    // no auto-create in this subset: make the index first
    let (status, _) = roundtrip(&addr, b"PUT /b HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(status, "200");

    // curl-shaped request: Expect + a >1KiB body, split across two
    // socket writes so the interim 100 lands between them.
    let pad = "x".repeat(2048);
    let doc = format!(r#"{{"title":"bulk {pad} body"}}"#);
    let body = format!(
        "{{\"index\":{{\"_index\":\"b\",\"_id\":\"1\"}}}}\n{doc}\n{{\"delete\":{{\"_index\":\"b\",\"_id\":\"9\"}}}}\n"
    );
    let head = format!(
        "POST /_bulk HTTP/1.1\r\nHost: x\r\nExpect: 100-continue\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut sock = TcpStream::connect(&addr).await.expect("connect");
    sock.write_all(head.as_bytes()).await.expect("write head");
    let mut interim = vec![0u8; 25]; // "HTTP/1.1 100 Continue\r\n\r\n"
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut interim))
        .await
        .expect("interim timeout")
        .expect("read interim");
    assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    sock.write_all(body.as_bytes()).await.expect("write body");

    let (status, body) = read_reply(&mut sock).await;
    assert_eq!(status, "200");
    assert!(has(&body, "\"errors\":false"), "{}", String::from_utf8_lossy(&body));
    assert!(has(&body, "\"result\":\"created\""), "{}", String::from_utf8_lossy(&body));
    // delete-miss: not_found status, but errors stays false
    assert!(has(&body, "\"result\":\"not_found\""), "{}", String::from_utf8_lossy(&body));
}

#[tokio::test]
async fn protocol_errors() {
    let shared = shared();
    let listener = bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    tokio::spawn(serve(listener, shared, String::new()));

    let (status, _) = roundtrip(&addr, b"GARBAGE\r\n\r\n").await;
    assert_eq!(status, "400");
    let (status, _) = roundtrip(&addr, b"GET /_nope HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, "404");
    let (status, _) = roundtrip(&addr, b"PUT /_bulk HTTP/1.1\r\n\r\n").await;
    assert_eq!(status, "405");
    // oversize declared body -> 413 before any body read
    let (status, _) = roundtrip(
        &addr,
        b"POST /_bulk HTTP/1.1\r\nContent-Length: 999999999\r\n\r\n",
    )
    .await;
    assert_eq!(status, "413");
    // chunked -> 501
    let (status, _) = roundtrip(
        &addr,
        b"POST /_bulk HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .await;
    assert_eq!(status, "501");
}
