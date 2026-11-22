//! Multi-store scenario #92 -- the night-shift handover belt on the
//! blocking move verbs the day-shift corpus never parked on: BLMOVE
//! LEFT/RIGHT carries the tail ticket onto the audit list in one wake
//! (reply = the moved bulk, dst lands it) and times out at its
//! deadline with the nil BULK $-1, while BRPOP serves a ready queue
//! IMMEDIATELY (tail-first *2) and parks out on an empty one with the
//! nil ARRAY *-1 -- the two timeout spellings pinned side by side.
//! One REAL node, RESP only (pipelined AUTH+block park).

mod common;
mod scenario_common;

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::contains_bytes;
use common::lite::frame;
use scenario_common::{front_world, int_of, r, rf, FrontSpec};

/// Park a pipelined AUTH + blocking command on one connection (the
/// +OK flushes BEFORE the block parks), wait out the race window,
/// then hand the socket back for the wake read.
async fn park(addr: &str, block: &[&[u8]]) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    let mut pipelined = frame(&[b"AUTH", common::TOKEN.as_bytes()]);
    pipelined.extend_from_slice(&frame(block));
    sock.write_all(&pipelined).await.expect("pipelined auth+block");
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), sock.read_exact(&mut hello))
        .await
        .expect("+OK flushed before the block parks")
        .expect("auth reply");
    assert_eq!(&hello, b"+OK\r\n");
    tokio::time::sleep(Duration::from_millis(200)).await;
    sock
}

/// Drain the socket until `needle` shows up or 5s pass; returns what
/// arrived (the caller asserts on the shape).
async fn drain_until(sock: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk))
            .await
            .expect("resolved within 5s")
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
async fn nightshift_handoff_moves_tail_tickets_and_clears_the_lane() {
    let (_front, a) = front_world("night92", &FrontSpec::default()).await;

    // a) BLMOVE LEFT/RIGHT wake: the reliever parks on the open lane,
    //    the day shift pushes the LAST ticket, and BLMOVE pops it from
    //    the head and lands it on the audit list in the same wake --
    //    the reply is the ticket bulk itself.
    let mut w1 = park(&a, &[b"BLMOVE", b"{ns}lane", b"{ns}audit", b"LEFT", b"RIGHT", b"5000"]).await;
    let started = Instant::now();
    assert_eq!(int_of(&r(&a, &[b"LPUSH", b"{ns}lane", b"ticket-77"]).await), 1);
    let moved = drain_until(&mut w1, b"ticket-77").await;
    assert!(moved.starts_with(b"$9\r\nticket-77\r\n"), "moved bulk: {moved:?}");
    assert!(started.elapsed() < Duration::from_secs(4), "wake, not timeout");
    let audit = rf(&a, &[b"LRANGE", b"{ns}audit", b"0", b"-1"]).await;
    assert!(contains_bytes(audit.as_bytes(), b"ticket-77"), "audit landed: {audit:?}");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{ns}lane"]).await), 0, "lane drained");

    // b) BLMOVE at its deadline: the nil BULK $-1 (reliable-delivery
    //    spelling, distinct from BLPOP's nil array).
    let parked_at = Instant::now();
    let mut w2 = park(&a, &[b"BLMOVE", b"{ns}lane", b"{ns}audit", b"LEFT", b"RIGHT", b"1"]).await;
    let timed_out = drain_until(&mut w2, b"$-1").await;
    assert!(timed_out.starts_with(b"$-1\r\n"), "nil bulk: {timed_out:?}");
    assert!(
        parked_at.elapsed() >= Duration::from_millis(1000),
        "it really waited out the 1s deadline: {:?}",
        parked_at.elapsed()
    );

    // c) BRPOP on a READY queue serves the tail immediately (no park):
    //    *2 with the queue name and the LAST-pushed value.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{ns}close", b"first"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{ns}close", b"last-out"]).await), 2);
    let mut s3 = park(&a, &[b"BRPOP", b"{ns}close", b"1"]).await;
    let sweep = drain_until(&mut s3, b"last-out").await;
    assert!(
        contains_bytes(&sweep, b"*2\r\n") && contains_bytes(&sweep, b"{ns}close")
            && contains_bytes(&sweep, b"last-out"),
        "brpop immediate tail: {sweep:?}"
    );
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{ns}close"]).await), 1, "tail gone");

    // d) BRPOP on an empty lane parks out at 1s with the nil ARRAY
    //    ({ns}close still holds "first"; the lane below is untouched).
    let parked_at = Instant::now();
    let mut w3 = park(&a, &[b"BRPOP", b"{ns}empty", b"1"]).await;
    let timed_out = drain_until(&mut w3, b"*-1").await;
    assert!(timed_out.starts_with(b"*-1\r\n"), "nil array: {timed_out:?}");
    assert!(parked_at.elapsed() >= Duration::from_millis(1000), "waited the deadline");

    // e) The one leftover "first" ticket moves RIGHT/LEFT (pop tail,
    //    push head) so the audit log reads newest-first: moved bulk
    //    again, audit order [first, ticket-77] -> head insert observed.
    let mut s4 = park(&a, &[b"BLMOVE", b"{ns}close", b"{ns}audit", b"RIGHT", b"LEFT", b"1"]).await;
    let moved2 = drain_until(&mut s4, b"first").await;
    assert!(moved2.starts_with(b"$5\r\nfirst\r\n"), "tail popped: {moved2:?}");
    let audit = rf(&a, &[b"LRANGE", b"{ns}audit", b"0", b"-1"]).await;
    assert!(
        contains_bytes(audit.as_bytes(), b"first\r\n$9\r\nticket-77"),
        "audit reads [first, ticket-77]: {audit:?}"
    );
}
