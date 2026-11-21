//! Multi-store scenario #33 -- the night-shift dispatch board on the
//! blocking list/zset verbs over REAL sockets: a pipelined AUTH+BLPOP
//! in one write parks (the +OK flushes first) and wakes on a push
//! inside the 4s window with the *2 (key, value) pair; an empty queue
//! BLPOPs out at its deadline with the nil ARRAY *-1; BRPOPLPUSH is
//! the reliable-delivery pattern (wake returns the job bulk and the
//! done-list carries it; its timeout is the nil BULK $-1); and the
//! urgent lane is a BZPOPMIN wake answering the flat *3
//! (key, member, score) triple. One REAL node, RESP only.

mod common;
mod scenario_common;

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::contains_bytes;
use common::lite::frame;
use scenario_common::{front_world, int_of, r, rf, FrontSpec};

/// Park a pipelined AUTH + blocking command on one connection; asserts
/// the +OK flushes BEFORE the block parks, waits out the race window,
/// then hands the socket back for the wake read.
async fn park(addr: &str, block: &[&[u8]]) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    let mut pipelined = frame(&[b"AUTH", common::TOKEN.as_bytes()]);
    pipelined.extend_from_slice(&frame(block));
    sock.write_all(&pipelined).await.expect("pipelined auth+block write");
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), sock.read_exact(&mut hello))
        .await
        .expect("+OK flushed before the block parks")
        .expect("auth reply");
    assert_eq!(&hello, b"+OK\r\n");
    tokio::time::sleep(Duration::from_millis(200)).await;
    sock
}

/// Drain the socket until `needle` shows up or 4s pass; returns what
/// arrived (the caller asserts on the shape).
async fn drain_until(sock: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(4), sock.read(&mut chunk))
            .await
            .expect("woken within 4s")
            .expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if contains_bytes(&buf, needle) {
            break;
        }
    }
    buf
}

#[tokio::test]
async fn night_dispatch_blocking_wakes_timeouts_and_priority_lane() {
    let (_front, a) = front_world("dispatch33", &FrontSpec::default()).await;

    // a) Worker 1 parks on the task queue; a dispatcher LPUSHes a job
    //    200ms later; the wake answers *2 (queue name, payload) well
    //    inside the 5s deadline.
    let mut w1 = park(&a, &[b"BLPOP", b"job:tasks", b"5000"]).await;
    let started = Instant::now();
    assert_eq!(int_of(&r(&a, &[b"LPUSH", b"job:tasks", b"fix-router"]).await), 1);
    let woke = drain_until(&mut w1, b"fix-router").await;
    assert!(
        contains_bytes(&woke, b"*2\r\n") && contains_bytes(&woke, b"job:tasks")
            && contains_bytes(&woke, b"fix-router"),
        "blpop wake shape: {woke:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(4), "wake, not timeout");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"job:tasks"]).await), 0, "queue drained");

    // b) An empty queue BLPOPs out at its 1s deadline with the nil
    //    ARRAY (not a nil bulk).
    let parked_at = Instant::now();
    let mut w2 = park(&a, &[b"BLPOP", b"job:empty", b"1"]).await;
    let timed_out = drain_until(&mut w2, b"*-1").await;
    assert!(timed_out.starts_with(b"*-1\r\n"), "nil array: {timed_out:?}");
    assert!(
        parked_at.elapsed() >= Duration::from_millis(1000),
        "it really waited out the 1s deadline: {:?}",
        parked_at.elapsed()
    );

    // c) Reliable delivery: BRPOPLPUSH moves the inbox job onto the
    //    done list in the same wake -- the reply is the JOB bulk and
    //    LRANGE sees it landed. src/dst share a {job} tag: the two-key
    //    verb is slot-checked even while parking.
    let mut w3 = park(&a, &[b"BRPOPLPUSH", b"{job}inbox", b"{job}done", b"5000"]).await;
    assert_eq!(int_of(&r(&a, &[b"LPUSH", b"{job}inbox", b"swap-disk"]).await), 1);
    let moved = drain_until(&mut w3, b"swap-disk").await;
    assert!(moved.starts_with(b"$9\r\nswap-disk\r\n"), "job bulk: {moved:?}");
    let done = rf(&a, &[b"LRANGE", b"{job}done", b"0", b"-1"]).await;
    assert!(done.contains("swap-disk"), "done list: {done:?}");

    // d) BRPOPLPUSH at its deadline: the nil BULK $-1 (distinct from
    //    BLPOP's nil array).
    let mut w4 = park(&a, &[b"BRPOPLPUSH", b"{job}inbox", b"{job}done", b"1"]).await;
    let timed_out = drain_until(&mut w4, b"$-1").await;
    assert!(timed_out.starts_with(b"$-1\r\n"), "nil bulk: {timed_out:?}");

    // e) The urgent lane: BZPOPMIN parks, a ZADD with score 9.5 wakes
    //    it with the flat *3 triple (key, member, score).
    let mut w5 = park(&a, &[b"BZPOPMIN", b"job:urgent", b"5000"]).await;
    let started = Instant::now();
    assert_eq!(int_of(&r(&a, &[b"ZADD", b"job:urgent", b"9.5", b"leak-mains"]).await), 1);
    let triple = drain_until(&mut w5, b"leak-mains").await;
    assert!(
        contains_bytes(&triple, b"*3\r\n") && contains_bytes(&triple, b"job:urgent")
            && contains_bytes(&triple, b"leak-mains") && contains_bytes(&triple, b"9.5"),
        "bzpopmin triple: {triple:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(4), "wake, not timeout");
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"job:urgent"]).await), 0, "popped out");
}
