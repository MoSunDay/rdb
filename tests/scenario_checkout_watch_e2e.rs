//! Multi-store scenario #34 -- flash-sale checkout under optimistic
//! concurrency: both buyers WATCH the same stock counter, the first
//! EXEC wins (queue replies +QUEUED, EXEC returns the per-command
//! replies array), the loser's EXEC aborts to the raw *-1 null array,
//! retries against the re-read stock and wins on the second round;
//! EXEC consumes the watch (a later unwatched transaction survives a
//! foreign write), and queue-time rejections are pinned verbatim
//! (CROSSSLOT without a shared tag, blocking commands, EXECABORT).
//! One REAL node, RESP only (persistent AUTH sessions).

mod common;
mod scenario_common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use scenario_common::{front_world, rb, r, FrontSpec};

/// A persistent AUTH'd session whose `cmd` returns the RAW reply bytes
/// (reads until the socket falls quiet: EXEC's nested arrays need it).
struct Session {
    sock: TcpStream,
}

impl Session {
    async fn connect(addr: &str) -> Session {
        let mut sock = TcpStream::connect(addr).await.expect("connect");
        let reply = cmd_raw(&mut sock, &[b"AUTH", common::TOKEN.as_bytes()]).await;
        assert_eq!(reply, b"+OK\r\n", "auth");
        Session { sock }
    }

    async fn cmd(&mut self, args: &[&[u8]]) -> Vec<u8> {
        cmd_raw(&mut self.sock, args).await
    }
}

/// Frame `args`, write once, drain the reply until a 120ms quiet gap.
async fn cmd_raw(sock: &mut TcpStream, args: &[&[u8]]) -> Vec<u8> {
    let mut frame = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        frame.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        frame.extend_from_slice(a);
        frame.extend_from_slice(b"\r\n");
    }
    sock.write_all(&frame).await.expect("write");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(Duration::from_millis(120), sock.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) | Err(_) => break,
        }
    }
    buf
}

#[tokio::test]
async fn flash_checkout_watch_conflict_abort_retry_and_queue_rejections() {
    let (_front, a) = front_world("watch34", &FrontSpec::default()).await;

    // a) The counter and the winner's cart share one hash tag so the
    //    MULTI body is slot-checked as a whole.
    assert_eq!(r(&a, &[b"SET", b"{co}stock", b"3"]).await, b"+OK");
    assert_eq!(r(&a, &[b"HSET", b"{co}cart:u1", b"item", b"drill"]).await, b":1");

    // b) Both buyers open their baskets on the SAME stock counter.
    let mut b1 = Session::connect(&a).await;
    let mut b2 = Session::connect(&a).await;
    assert_eq!(b1.cmd(&[b"WATCH", b"{co}stock"]).await, b"+OK\r\n");
    assert_eq!(b2.cmd(&[b"WATCH", b"{co}stock"]).await, b"+OK\r\n");

    // c) Buyer 1 commits first: both verbs queue (+QUEUED) and EXEC
    //    replies with the per-command array -- :2 is the new stock,
    //    :1 the cart's paid field.
    assert_eq!(b1.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(b1.cmd(&[b"DECR", b"{co}stock"]).await, b"+QUEUED\r\n");
    assert_eq!(b1.cmd(&[b"HSET", b"{co}cart:u1", b"paid", b"1"]).await, b"+QUEUED\r\n");
    let exec1 = b1.cmd(&[b"EXEC"]).await;
    let exec1 = String::from_utf8_lossy(&exec1).into_owned();
    assert!(
        exec1.starts_with("*2\r\n") && exec1.contains(":2\r\n") && exec1.contains(":1\r\n"),
        "exec replies: {exec1:?}"
    );

    // d) Buyer 2's basket is stale: EXEC aborts to the raw null array
    //    (*-1) and writes NOTHING (stock still 2 -- buyer 1's paid
    //    field survives untouched, the loser added no state of his own).
    assert_eq!(b2.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(b2.cmd(&[b"DECR", b"{co}stock"]).await, b"+QUEUED\r\n");
    assert_eq!(b2.cmd(&[b"EXEC"]).await, b"*-1\r\n", "aborted");
    assert_eq!(rb(&a, &[b"GET", b"{co}stock"]).await, "2");
    assert_eq!(r(&a, &[b"HGET", b"{co}cart:u1", b"paid"]).await, b"$1\r\n1");
    assert_eq!(r(&a, &[b"HGET", b"{co}cart:u1", b"paid2"]).await, b"$-1", "loser wrote nothing");

    // e) The retry loop: re-WATCH the re-read value, transact, win.
    assert_eq!(b2.cmd(&[b"WATCH", b"{co}stock"]).await, b"+OK\r\n");
    assert_eq!(b2.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(b2.cmd(&[b"DECR", b"{co}stock"]).await, b"+QUEUED\r\n");
    let exec2 = b2.cmd(&[b"EXEC"]).await;
    let exec2 = String::from_utf8_lossy(&exec2).into_owned();
    assert!(exec2.starts_with("*1\r\n") && exec2.contains(":1\r\n"), "{exec2:?}");
    assert_eq!(rb(&a, &[b"GET", b"{co}stock"]).await, "1", "two sold");

    // f) EXEC consumed the watch: an UNWATCHED transaction now survives
    //    a foreign write between queue and commit.
    assert_eq!(b2.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(b2.cmd(&[b"SET", b"{co}note", b"restock"]).await, b"+QUEUED\r\n");
    assert_eq!(r(&a, &[b"SET", b"{co}stock", b"5"]).await, b"+OK");
    assert_eq!(b2.cmd(&[b"EXEC"]).await, b"*1\r\n+OK\r\n", "no watch, no abort");
    assert_eq!(rb(&a, &[b"GET", b"{co}note"]).await, "restock");

    // g) Queue-time rejections, verbatim: untagged multi-key verbs
    //    CROSSSLOT at queue time (the first queued verb decides the
    //    slot), blocking commands never queue, and a poisoned queue
    //    aborts the whole EXEC.
    let mut s = Session::connect(&a).await;
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(s.cmd(&[b"SET", b"foo", b"1"]).await, b"+QUEUED\r\n");
    assert_eq!(
        s.cmd(&[b"SET", b"bar", b"2"]).await,
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n"
    );
    assert!(s.cmd(&[b"EXEC"]).await.starts_with(b"-EXECABORT"));
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        s.cmd(&[b"BLPOP", b"foo", b"1"]).await,
        b"-ERR command 'blpop' is not allowed in transactions\r\n"
    );
    assert!(s.cmd(&[b"EXEC"]).await.starts_with(b"-EXECABORT"));

    // h) The winner's cart survived every abort untouched.
    assert_eq!(rb(&a, &[b"HGET", b"{co}cart:u1", b"item"]).await, "drill");
}
