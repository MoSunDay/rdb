//! Multi-store scenario #80 -- an auction standby board on the blocking
//! zset verbs over REAL sockets: BZPOPMIN answers the flat *3 triple
//! (key, member, score) immediately on a ready key, an empty board
//! parks to its deadline and answers the null ARRAY *-1, BLOCK 0 parks
//! forever until a spawned writer's ZADD wakes it, BZPOPMAX takes the
//! HIGHEST score symmetrically, a multi-key scan skips the empty first
//! key (same-slot pair) and pops the second, and a non-float timeout
//! refuses verbatim. One REAL node, RESP only.

use crate::common;
use crate::scenario_common;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::contains_bytes;
use scenario_common::{front_world, int_of, r, rf, rt, FrontSpec};

/// Park a pipelined AUTH + blocking command on one connection; asserts
/// the +OK flushes BEFORE the block parks, waits out the race window,
/// then hands the socket back for the wake read.
async fn park(addr: &str, block: &[&[u8]]) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    let mut pipelined = common::lite::frame(&[b"AUTH", common::TOKEN.as_bytes()]);
    pipelined.extend_from_slice(&common::lite::frame(block));
    sock.write_all(&pipelined)
        .await
        .expect("pipelined auth+block write");
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), sock.read_exact(&mut hello))
        .await
        .expect("+OK flushed before the block parks")
        .expect("auth reply");
    assert_eq!(&hello, b"+OK\r\n");
    tokio::time::sleep(Duration::from_millis(200)).await;
    sock
}

/// Drain the socket until `needle` shows up or 4s pass.
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
async fn auction_standby_immediate_timeout_wake_max_and_multiscan() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("auc80", &FrontSpec::default()).await;

    // a) Immediate hit on a ready board: the flat *3 triple echoes the
    //    score as written.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{au}board", b"1.5", b"bid-lo"]).await),
        1
    );
    assert_eq!(
        rf(&a, &[b"BZPOPMIN", b"{au}board", b"1"]).await,
        "*3\r\n$9\r\n{au}board\r\n$6\r\nbid-lo\r\n$3\r\n1.5\r\n",
        "flat triple, score verbatim"
    );

    // b) The drained board parks to its deadline: null ARRAY *-1,
    //    honestly waited (~200ms of parking, no early bail).
    let started = Instant::now();
    assert_eq!(
        rf(&a, &[b"BZPOPMIN", b"{au}board", b"0.2"]).await,
        "*-1\r\n"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "parked, not instant"
    );

    // c) BLOCK 0 parks forever: a spawned writer's ZADD 300ms later
    //    wakes the standby with the triple.
    let mut late = park(&a, &[b"BZPOPMIN", b"{au}late", b"0"]).await;
    let waker = a.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            int_of(&r(&waker, &[b"ZADD", b"{au}late", b"7", b"sniper"]).await),
            1
        );
    });
    let woke = drain_until(&mut late, b"sniper").await;
    writer.await.expect("writer");
    assert!(
        contains_bytes(&woke, b"*3\r\n")
            && contains_bytes(&woke, b"sniper")
            && contains_bytes(&woke, b"$1\r\n7\r\n"),
        "wake triple: {woke:?}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZCARD", b"{au}late"]).await),
        0,
        "popped out"
    );

    // d) BZPOPMAX twin: the HIGHEST score leaves first.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{au}top", b"2", b"low", b"9.5", b"high"]).await),
        2
    );
    let top = rf(&a, &[b"BZPOPMAX", b"{au}top", b"1"]).await;
    assert!(
        top.contains("*3") && top.ends_with("high\r\n$3\r\n9.5\r\n"),
        "max first: {top}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZCARD", b"{au}top"]).await),
        1,
        "low survives"
    );

    // e) Multi-key same-slot scan: the empty standby board is skipped,
    //    the second key serves the pop and names ITSELF in the triple.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{au}alt", b"3.3", b"fallback"]).await),
        1
    );
    let multi = rf(&a, &[b"BZPOPMIN", b"{au}board", b"{au}alt", b"1"]).await;
    assert!(
        multi.contains("*3") && multi.contains("{au}alt") && multi.contains("fallback"),
        "{multi}"
    );
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{au}alt"]).await), 0);

    // f) A non-float timeout refuses verbatim (both ends).
    assert_eq!(
        rt(&a, &[b"BZPOPMIN", b"{au}board", b"xyz"]).await,
        "-ERR timeout is not a float or out of range"
    );
    assert_eq!(
        rt(&a, &[b"BZPOPMAX", b"{au}board", b"soon"]).await,
        "-ERR timeout is not a float or out of range"
    );
}
