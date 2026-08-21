//! Conn-level regression tests for approved Redis-semantics alignments:
//! QUIT replies exactly one `+OK`; missing-key commands reply the
//! Redis-standard arity error (no fabricated Go panic text); an empty
//! multibulk (`*0`) gets the same arity treatment with an empty name.
//!
//! Helpers mirror `resp_e2e.rs` (`state::testutil` is lib-internal).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{conf, monitor, resp, state, store, topology};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN: &str = "test-token";

fn test_shared(tag: &str) -> Arc<state::Shared> {
    let c = conf::Config {
        bind: "127.0.0.1:32681".to_string(),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: "127.0.0.1:22681".to_string(),
        raft_token: TOKEN.to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &c.bind);
    let st = store::open(path.to_str().unwrap()).unwrap();
    Arc::new(state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(&c))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: std::sync::Arc::new(rdb::lite::new_runtime()),
        sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
        conf: c,
    })
}

fn resp_req(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        v.extend_from_slice(p);
        v.extend_from_slice(b"\r\n");
    }
    v
}

async fn authed_conn(addr: &std::net::SocketAddr, tag: &str) -> TcpStream {
    let mut s = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    s.write_all(&resp_req(&[b"AUTH", TOKEN.as_bytes()]))
        .await
        .expect("auth write");
    let mut ok = [0u8; 5];
    tokio::time::timeout(TIMEOUT, s.read_exact(&mut ok))
        .await
        .expect("auth read timeout")
        .expect("auth read");
    assert_eq!(&ok, b"+OK\r\n", "{tag}: auth failed");
    s
}

/// Write `req`, read exactly `expect.len()` bytes, byte-compare.
async fn rpc(sock: &mut TcpStream, req: &[u8], expect: &[u8], tag: &str) {
    sock.write_all(req).await.expect("write");
    let mut buf = vec![0u8; expect.len()];
    tokio::time::timeout(TIMEOUT, sock.read_exact(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("{tag}: read timed out, expected {expect:?}"))
        .unwrap_or_else(|e| panic!("{tag}: read failed: {e}"));
    assert_eq!(buf, expect, "{tag}");
}

/// Drain until EOF; returns everything the server sent past our reads.
async fn drain_to_eof(sock: &mut TcpStream) -> Vec<u8> {
    let mut all = Vec::new();
    let mut b = [0u8; 64];
    loop {
        let n = tokio::time::timeout(TIMEOUT, sock.read(&mut b))
            .await
            .expect("eof read timed out")
            .expect("eof read failed");
        if n == 0 {
            return all;
        }
        all.extend_from_slice(&b[..n]);
    }
}

async fn spawn_server(tag: &str) -> std::net::SocketAddr {
    let shared = test_shared(tag);
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared));
    addr
}

#[tokio::test]
async fn quit_replies_exactly_one_ok_then_closes() {
    let addr = spawn_server("quit-one-ok").await;
    let mut s = authed_conn(&addr, "quit").await;
    rpc(&mut s, &resp_req(&[b"quit"]), b"+OK\r\n", "quit").await;
    // Exactly one reply: nothing else (no +PONG) may precede the EOF.
    assert_eq!(drain_to_eof(&mut s).await, b"");
}

#[tokio::test]
async fn missing_key_command_replies_arity_error() {
    let addr = spawn_server("arity-missing-key").await;
    let mut s = authed_conn(&addr, "arity").await;
    rpc(
        &mut s,
        &resp_req(&[b"get"]),
        b"-ERR wrong number of arguments for 'get' command\r\n",
        "lone get",
    )
    .await;
    // Uppercase input still names the command in lowercase (Redis style).
    rpc(
        &mut s,
        &resp_req(&[b"DEL"]),
        b"-ERR wrong number of arguments for 'del' command\r\n",
        "lone DEL",
    )
    .await;
    // No fabricated Go panic text anywhere; connection stays usable.
    rpc(
        &mut s,
        &resp_req(&[b"ping"]),
        b"+PONG\r\n",
        "post-error ping",
    )
    .await;
}

#[tokio::test]
async fn empty_multibulk_replies_arity_error_with_empty_name() {
    let addr = spawn_server("arity-empty-multibulk").await;
    let mut s = authed_conn(&addr, "empty-multibulk").await;
    rpc(
        &mut s,
        b"*0\r\n",
        b"-ERR wrong number of arguments for '' command\r\n",
        "*0",
    )
    .await;
    rpc(&mut s, &resp_req(&[b"ping"]), b"+PONG\r\n", "post-*0 ping").await;
}
