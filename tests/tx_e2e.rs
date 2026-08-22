//! MULTI/EXEC/DISCARD/WATCH/UNWATCH end-to-end over a real rdb process:
//! the RESP layer queueing, the EXEC engine, WATCH conflicts via a second
//! connection, and the tx metrics.

mod common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{spawn_node, wait_resp_ready, TOKEN};

/// Persistent AUTHed connection: sequential command/reply exchange.
struct Session {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Session {
    async fn connect(addr: &str) -> Session {
        let mut s = Session {
            sock: TcpStream::connect(addr).await.expect("connect"),
            buf: Vec::new(),
        };
        let ok = s.cmd(&[b"AUTH", TOKEN.as_bytes()]).await;
        assert_eq!(ok, "+OK", "auth");
        s
    }

    async fn cmd(&mut self, args: &[&[u8]]) -> String {
        let mut frame = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            frame.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            frame.extend_from_slice(a);
            frame.extend_from_slice(b"\r\n");
        }
        self.sock.write_all(&frame).await.expect("write");
        self.read_frame().await.expect("reply frame")
    }

    /// Read one fully-buffered reply frame, rendered as a readable string:
    /// `+OK`, `-ERR ...`, `:1`, `$-1`, `$3 abc`, `*2 [+OK, $1 v]`, `*-1`.
    async fn read_frame(&mut self) -> Option<String> {
        loop {
            if let Some(rendered) = try_parse(&mut self.buf) {
                return Some(rendered);
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(5), self.sock.read(&mut chunk))
                .await
                .ok()?
                .ok()?;
            if n == 0 {
                return None;
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// Parse one frame from the front of `buf`, consuming it; `None` when the
/// frame is not fully buffered yet.
fn try_parse(buf: &mut Vec<u8>) -> Option<String> {
    let line_end = buf.windows(2).position(|w| w == b"\r\n")?;
    let header = String::from_utf8_lossy(&buf[..line_end]).into_owned();
    let (kind, rest) = header.split_at(1);
    match kind {
        "+" | "-" | ":" => {
            buf.drain(..line_end + 2);
            Some(header)
        }
        "$" => {
            let len: i64 = rest.parse().ok()?;
            if len < 0 {
                buf.drain(..line_end + 2);
                return Some(header);
            }
            let len = len as usize;
            let total = line_end + 2 + len + 2;
            if buf.len() < total {
                return None;
            }
            let payload =
                String::from_utf8_lossy(&buf[line_end + 2..line_end + 2 + len]).into_owned();
            buf.drain(..total);
            Some(format!("${} {}", len, payload))
        }
        "*" => {
            let count: i64 = rest.parse().ok()?;
            if count < 0 {
                buf.drain(..line_end + 2);
                return Some(header);
            }
            // Recurse over children against a scratch view: parse child
            // frames one by one, rolling the consumed prefix forward.
            buf.drain(..line_end + 2);
            let mut parts = Vec::new();
            for _ in 0..count {
                let part = try_parse(buf)?;
                parts.push(part);
            }
            Some(format!("*{} [{}]", count, parts.join(", ")))
        }
        _ => Some(header), // unknown: surface raw line
    }
}

async fn setup(tag: &str) -> common::ProcNode {
    let dir = std::env::temp_dir().join(format!("rdb-tx-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    node
}

#[tokio::test]
async fn multi_exec_queues_and_replays() {
    let mut node = setup("basic").await;
    let mut s = Session::connect(&node.resp).await;
    assert_eq!(s.cmd(&[b"MULTI"]).await, "+OK");
    assert_eq!(s.cmd(&[b"SET", b"txk", b"v1"]).await, "+QUEUED");
    assert_eq!(s.cmd(&[b"GET", b"txk"]).await, "+QUEUED");
    // replies: SET -> +OK, GET -> bulk v1
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*2 [+OK, $2 v1]");
    assert_eq!(s.cmd(&[b"GET", b"txk"]).await, "$2 v1");
    node.child.kill().ok();
}

#[tokio::test]
async fn exec_discard_without_multi_error() {
    let mut node = setup("nometx").await;
    let mut s = Session::connect(&node.resp).await;
    assert_eq!(s.cmd(&[b"EXEC"]).await, "-ERR EXEC without MULTI");
    assert_eq!(s.cmd(&[b"DISCARD"]).await, "-ERR DISCARD without MULTI");
    node.child.kill().ok();
}

#[tokio::test]
async fn discard_throws_queue_away() {
    let mut node = setup("discard").await;
    let mut s = Session::connect(&node.resp).await;
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"dk", b"v"]).await;
    assert_eq!(s.cmd(&[b"DISCARD"]).await, "+OK");
    assert_eq!(s.cmd(&[b"GET", b"dk"]).await, "$-1");
    node.child.kill().ok();
}

#[tokio::test]
async fn dirty_transaction_execaborts_and_executes_nothing() {
    let mut node = setup("dirty").await;
    let mut s = Session::connect(&node.resp).await;
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"dk", b"v"]).await;
    // unknown command: error now + dirty
    assert_eq!(
        s.cmd(&[b"NOSUCHCMD", b"x"]).await,
        "-ERR unknown command 'NOSUCHCMD'"
    );
    // later commands still queue (same key: cross-slot is a separate rule)
    assert_eq!(s.cmd(&[b"SET", b"dk", b"v2"]).await, "+QUEUED");
    assert_eq!(
        s.cmd(&[b"EXEC"]).await,
        "-EXECABORT Transaction discarded because of previous errors."
    );
    // NOTHING executed
    assert_eq!(s.cmd(&[b"GET", b"dk"]).await, "$-1");
    assert_eq!(s.cmd(&[b"GET", b"dk"]).await, "$-1");
    // state reset: EXEC again is an error, not a replay
    assert_eq!(s.cmd(&[b"EXEC"]).await, "-ERR EXEC without MULTI");
    node.child.kill().ok();
}

#[tokio::test]
async fn crossslot_and_blocking_reject_at_queue_time() {
    let mut node = setup("reject").await;
    let mut s = Session::connect(&node.resp).await;
    s.cmd(&[b"MULTI"]).await;
    // different slots (no shared hash tag)
    s.cmd(&[b"SET", b"foo", b"1"]).await;
    assert_eq!(
        s.cmd(&[b"SET", b"bar", b"2"]).await,
        "-ERR CROSSSLOT Keys in request don't hash to the same slot"
    );
    assert!(s.cmd(&[b"EXEC"]).await.starts_with("-EXECABORT"));
    // blocking command
    s.cmd(&[b"MULTI"]).await;
    assert_eq!(
        s.cmd(&[b"BLPOP", b"foo", b"1"]).await,
        "-ERR command 'blpop' is not allowed in transactions"
    );
    assert!(s.cmd(&[b"EXEC"]).await.starts_with("-EXECABORT"));
    // nested MULTI + WATCH inside MULTI
    s.cmd(&[b"MULTI"]).await;
    assert_eq!(
        s.cmd(&[b"MULTI"]).await,
        "-ERR MULTI calls can not be nested"
    );
    assert_eq!(
        s.cmd(&[b"WATCH", b"foo"]).await,
        "-ERR WATCH inside MULTI is not allowed"
    );
    assert!(s.cmd(&[b"EXEC"]).await.starts_with("-EXECABORT"));
    node.child.kill().ok();
}

#[tokio::test]
async fn hash_tag_multi_key_transaction_commits() {
    let mut node = setup("hashtag").await;
    let mut s = Session::connect(&node.resp).await;
    s.cmd(&[b"MULTI"]).await;
    assert_eq!(s.cmd(&[b"SADD", b"{u}a", b"m1"]).await, "+QUEUED");
    assert_eq!(s.cmd(&[b"SADD", b"{u}b", b"m2"]).await, "+QUEUED");
    assert_eq!(s.cmd(&[b"SUNION", b"{u}a", b"{u}b"]).await, "+QUEUED");
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*3 [:1, :1, *2 [$2 m1, $2 m2]]");
    node.child.kill().ok();
}

#[tokio::test]
async fn watch_aborts_on_foreign_write_and_clears_after() {
    let mut node = setup("watch").await;
    let mut s = Session::connect(&node.resp).await;
    let mut other = Session::connect(&node.resp).await;
    assert_eq!(s.cmd(&[b"WATCH", b"wk"]).await, "+OK");
    // a write on ANOTHER connection between WATCH and EXEC
    assert_eq!(other.cmd(&[b"SET", b"wk", b"foreign"]).await, "+OK");
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"wk", b"mine"]).await;
    // null array: aborted, nothing executed
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*-1");
    assert_eq!(s.cmd(&[b"GET", b"wk"]).await, "$7 foreign");
    // EXEC consumed the watch: a second transaction now succeeds
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"wk", b"mine"]).await;
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*1 [+OK]");
    assert_eq!(s.cmd(&[b"GET", b"wk"]).await, "$4 mine");
    node.child.kill().ok();
}

#[tokio::test]
async fn unwatch_and_own_write_unwatch() {
    let mut node = setup("unwatch").await;
    let mut s = Session::connect(&node.resp).await;
    let mut other = Session::connect(&node.resp).await;
    // UNWATCH clears: foreign write no longer aborts
    assert_eq!(s.cmd(&[b"WATCH", b"k1"]).await, "+OK");
    assert_eq!(s.cmd(&[b"UNWATCH"]).await, "+OK");
    other.cmd(&[b"SET", b"k1", b"x"]).await;
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"k1", b"y"]).await;
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*1 [+OK]");
    // own write outside MULTI also implicitly unwatches
    s.cmd(&[b"WATCH", b"k2"]).await;
    assert_eq!(s.cmd(&[b"SET", b"k2", b"a"]).await, "+OK");
    other.cmd(&[b"SET", b"k2", b"b"]).await;
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"k2", b"c"]).await;
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*1 [+OK]");
    node.child.kill().ok();
}

#[tokio::test]
async fn empty_multi_exec_is_empty_array() {
    let mut node = setup("empty").await;
    let mut s = Session::connect(&node.resp).await;
    s.cmd(&[b"MULTI"]).await;
    assert_eq!(s.cmd(&[b"EXEC"]).await, "*0 []");
    node.child.kill().ok();
}

/// Minimal GET /metrics (Connection: close) reader.
async fn http_get_metrics(addr: &str) -> Vec<u8> {
    let mut sock = TcpStream::connect(addr).await.expect("monitor connect");
    let req = format!("GET /metrics HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.expect("write");
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

#[tokio::test]
async fn tx_metrics_exposed() {
    let mut node = setup("metrics").await;
    let mut s = Session::connect(&node.resp).await;
    s.cmd(&[b"MULTI"]).await;
    s.cmd(&[b"SET", b"mk", b"v"]).await;
    s.cmd(&[b"EXEC"]).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let buf = loop {
        let buf = http_get_metrics(&node.monitor).await;
        if buf.windows(13).any(|w| w == b"rdb_tx_events".as_slice())
            || std::time::Instant::now() > deadline
        {
            break buf;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        buf.windows(13).any(|w| w == b"rdb_tx_events".as_slice()),
        "metrics carry rdb_tx_events"
    );
    node.child.kill().ok();
}
