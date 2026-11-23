//! Multi-store scenario #93 -- the counter order-desk aborting edits:
//! DISCARD throws the queued writes away (nothing lands, the session
//! leaves MULTI so a second DISCARD draws the without-MULTI error),
//! DISCARD also drops the watches (an EXEC after it survives a
//! foreign write), UNWATCH alone clears watches the same way, and a
//! nested MULTI marks the queue dirty -- the abort surface (#34 raced
//! EXEC; DISCARD/UNWATCH never appeared). One REAL node, RESP only
//! (persistent AUTH session over one socket).

use crate::common;
use crate::scenario_common;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use scenario_common::{front_world, r, rb, FrontSpec};

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
            Ok(Err(_)) => break,
            Err(_) => break, // fell quiet: reply complete
        }
    }
    buf
}

#[tokio::test]
async fn cashier_aborts_edits_and_unwatches_before_committing() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("cash93", &FrontSpec::default()).await;
    assert!(r(&a, &[b"SET", b"{co}total", b"120"])
        .await
        .starts_with(b"+OK"));
    let mut s = Session::connect(&a).await;

    // a) Queue an edit, then regret it: DISCARD drops the whole queue
    //    -- the counter total is untouched and the session is OUT of
    //    MULTI (a second DISCARD draws the without-MULTI error, and
    //    EXEC does too).
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(s.cmd(&[b"SET", b"{co}total", b"999"]).await, b"+QUEUED\r\n");
    assert_eq!(
        s.cmd(&[b"HSET", b"{co}cart:u9", b"item", b"void"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(s.cmd(&[b"DISCARD"]).await, b"+OK\r\n", "abort the edits");
    assert_eq!(
        rb(&a, &[b"GET", b"{co}total"]).await,
        "120",
        "queue never landed"
    );
    assert_eq!(
        rb(&a, &[b"HGET", b"{co}cart:u9", b"item"]).await,
        "",
        "no cart either"
    );
    assert!(
        s.cmd(&[b"DISCARD"])
            .await
            .starts_with(b"-ERR DISCARD without MULTI"),
        "left MULTI"
    );
    assert!(
        s.cmd(&[b"EXEC"])
            .await
            .starts_with(b"-ERR EXEC without MULTI"),
        "exec without multi"
    );

    // b) After DISCARD the session transacts cleanly again (the dirty
    //    flag from any nested-MULTI Mark does not leak): a fresh MULTI
    //    queues and EXECs.
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        s.cmd(&[b"INCRBY", b"{co}total", b"5"]).await,
        b"+QUEUED\r\n"
    );
    let done = s.cmd(&[b"EXEC"]).await;
    assert!(
        common::contains_bytes(&done, b":125"),
        "commit after abort: {done:?}"
    );

    // c) Nested MULTI is refused and DIRTS the open transaction: the
    //    queued write aborts at EXEC (EXECABORT), nothing lands.
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(s.cmd(&[b"SET", b"{co}total", b"0"]).await, b"+QUEUED\r\n");
    assert_eq!(
        s.cmd(&[b"MULTI"]).await,
        b"-ERR MULTI calls can not be nested\r\n"
    );
    assert!(
        s.cmd(&[b"EXEC"]).await.starts_with(b"-EXECABORT"),
        "poisoned"
    );
    assert_eq!(rb(&a, &[b"GET", b"{co}total"]).await, "125", "still 125");

    // d) UNWATCH clears watches on its own: watch the total, suffer a
    //    foreign write, UNWATCH, then EXEC survives.
    assert_eq!(s.cmd(&[b"WATCH", b"{co}total"]).await, b"+OK\r\n");
    assert!(
        r(&a, &[b"SET", b"{co}total", b"130"])
            .await
            .starts_with(b"+OK"),
        "foreign write"
    );
    assert_eq!(s.cmd(&[b"UNWATCH"]).await, b"+OK\r\n");
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        s.cmd(&[b"INCRBY", b"{co}total", b"10"]).await,
        b"+QUEUED\r\n"
    );
    let survived = s.cmd(&[b"EXEC"]).await;
    assert!(
        common::contains_bytes(&survived, b":140"),
        "unwatched exec survives foreign write: {survived:?}"
    );

    // e) Without the UNWATCH the same dance would abort (control for
    //    (d), pinning why UNWATCH matters): re-watch, foreign write,
    //    EXEC -> the null array.
    assert_eq!(s.cmd(&[b"WATCH", b"{co}total"]).await, b"+OK\r\n");
    assert!(r(&a, &[b"SET", b"{co}total", b"150"])
        .await
        .starts_with(b"+OK"));
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        s.cmd(&[b"INCRBY", b"{co}total", b"1"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(s.cmd(&[b"EXEC"]).await, b"*-1\r\n", "watched abort");
    assert_eq!(
        rb(&a, &[b"GET", b"{co}total"]).await,
        "150",
        "abort left it"
    );

    // f) DISCARD unwatches too: watch, foreign write, MULTI, DISCARD,
    //    then a fresh transaction commits despite the stale watch.
    assert_eq!(s.cmd(&[b"WATCH", b"{co}total"]).await, b"+OK\r\n");
    assert!(r(&a, &[b"SET", b"{co}total", b"160"])
        .await
        .starts_with(b"+OK"));
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(s.cmd(&[b"DISCARD"]).await, b"+OK\r\n", "discard unwatches");
    assert_eq!(s.cmd(&[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        s.cmd(&[b"SET", b"{co}closed", b"yes"]).await,
        b"+QUEUED\r\n"
    );
    let final_exec = s.cmd(&[b"EXEC"]).await;
    assert!(
        common::contains_bytes(&final_exec, b"+OK"),
        "commit after discard: {final_exec:?}"
    );
    assert_eq!(rb(&a, &[b"GET", b"{co}closed"]).await, "yes");
}
