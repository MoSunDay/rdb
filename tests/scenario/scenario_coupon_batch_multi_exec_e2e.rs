//! Multi-store scenario #74 -- a coupon batch issue inside MULTI/EXEC
//! on a persistent AUTH'd session: every verb queues (+QUEUED), EXEC
//! replays the per-command replies in submission order, an empty queue
//! answers *0, EXEC without MULTI and nested MULTI refuse verbatim, a
//! queue-time rejection poisons the whole batch (EXECABORT discards
//! everything), and a RUNTIME error rides inline WITHOUT aborting its
//! siblings. Every key shares the {c74} tag: the MULTI body is
//! slot-checked as a whole. One REAL node, RESP only.

use crate::common;
use crate::scenario_common;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use scenario_common::{front_world, r, rb, FrontSpec};

/// Open a persistent AUTH'd session.
async fn sess_open(addr: &str) -> TcpStream {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    assert_eq!(
        sess_cmd(&mut sock, &[b"AUTH", common::TOKEN.as_bytes()]).await,
        b"+OK\r\n"
    );
    sock
}

/// One command per round trip; the FIRST byte may lag on a loaded box
/// (wait up to 3s), then the reply drains until a 150ms quiet gap
/// (EXEC's nested arrays arrive in pieces).
async fn sess_cmd(sock: &mut TcpStream, args: &[&[u8]]) -> Vec<u8> {
    use std::time::Instant;
    sock.write_all(&common::lite::frame(args))
        .await
        .expect("write");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let wait = if buf.is_empty() {
            deadline.saturating_duration_since(Instant::now())
        } else {
            Duration::from_millis(150)
        };
        if wait.is_zero() {
            break;
        }
        match tokio::time::timeout(wait, sock.read(&mut chunk)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) | Err(_) => break,
        }
    }
    buf
}

#[tokio::test]
async fn coupon_batch_queues_replays_and_aborts_wholesale() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("coupon74", &FrontSpec::default()).await;

    // a) EXEC without MULTI, verbatim.
    let mut s0 = sess_open(&a).await;
    assert_eq!(
        sess_cmd(&mut s0, &[b"EXEC"]).await,
        b"-ERR EXEC without MULTI\r\n"
    );

    // b) The happy batch: verbs queue in order, EXEC replays each
    //    verb's typed reply in submission order.
    let mut s1 = sess_open(&a).await;
    assert_eq!(sess_cmd(&mut s1, &[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        sess_cmd(&mut s1, &[b"SET", b"{c74}batch:1", b"SAVE10"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(
        sess_cmd(&mut s1, &[b"INCR", b"{c74}issued"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(
        sess_cmd(&mut s1, &[b"LPUSH", b"{c74}ledger", b"save-10"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(
        sess_cmd(&mut s1, &[b"EXEC"]).await,
        b"*3\r\n+OK\r\n:1\r\n:1\r\n",
        "ordered replay"
    );
    assert_eq!(rb(&a, &[b"GET", b"{c74}batch:1"]).await, "SAVE10");
    assert_eq!(rb(&a, &[b"LINDEX", b"{c74}ledger", b"0"]).await, "save-10");

    // c) Empty queue: MULTI then EXEC answers the empty array *0.
    let mut s2 = sess_open(&a).await;
    assert_eq!(sess_cmd(&mut s2, &[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        sess_cmd(&mut s2, &[b"EXEC"]).await,
        b"*0\r\n",
        "empty transaction"
    );

    // d) Nested MULTI refuses AND dirties: the queued writes never
    //    land and the session leaves MULTI afterwards.
    let mut s3 = sess_open(&a).await;
    assert_eq!(sess_cmd(&mut s3, &[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        sess_cmd(&mut s3, &[b"SET", b"{c74}nested", b"v"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(
        sess_cmd(&mut s3, &[b"MULTI"]).await,
        b"-ERR MULTI calls can not be nested\r\n"
    );
    assert_eq!(
        sess_cmd(&mut s3, &[b"EXEC"]).await,
        b"-EXECABORT Transaction discarded because of previous errors.\r\n"
    );
    assert_eq!(
        r(&a, &[b"GET", b"{c74}nested"]).await,
        b"$-1",
        "discarded writes never land"
    );
    assert_eq!(
        sess_cmd(&mut s3, &[b"SET", b"{c74}after", b"ok"]).await,
        b"+OK\r\n",
        "session freed"
    );

    // e) Queue-time poison: an unknown verb at queue time dirties the
    //    batch; EXEC discards the healthy siblings too.
    let mut s4 = sess_open(&a).await;
    assert_eq!(sess_cmd(&mut s4, &[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        sess_cmd(&mut s4, &[b"SET", b"{c74}poison", b"v"]).await,
        b"+QUEUED\r\n"
    );
    assert!(sess_cmd(&mut s4, &[b"NOSUCHVERB", b"x"])
        .await
        .starts_with(b"-ERR unknown command"));
    assert_eq!(
        sess_cmd(&mut s4, &[b"EXEC"]).await,
        b"-EXECABORT Transaction discarded because of previous errors.\r\n"
    );
    assert_eq!(
        r(&a, &[b"GET", b"{c74}poison"]).await,
        b"$-1",
        "poisoned batch wrote nothing"
    );

    // f) A RUNTIME error rides inline and its siblings still commit:
    //    INCRBY on a non-numeric string fails mid-EXEC, the fresh
    //    counter after it still lands.
    assert_eq!(r(&a, &[b"SET", b"{c74}whoami", b"not-a-num"]).await, b"+OK");
    let mut s5 = sess_open(&a).await;
    assert_eq!(sess_cmd(&mut s5, &[b"MULTI"]).await, b"+OK\r\n");
    assert_eq!(
        sess_cmd(&mut s5, &[b"INCRBY", b"{c74}whoami", b"5"]).await,
        b"+QUEUED\r\n"
    );
    assert_eq!(
        sess_cmd(&mut s5, &[b"INCR", b"{c74}fresh"]).await,
        b"+QUEUED\r\n"
    );
    let inline = sess_cmd(&mut s5, &[b"EXEC"]).await;
    let inline = String::from_utf8_lossy(&inline).into_owned();
    assert!(
        inline.starts_with("*2\r\n")
            && inline.contains("-ERR value is not an integer or out of range")
            && inline.contains(":1\r\n"),
        "runtime error inline, sibling lives: {inline:?}"
    );
    assert_eq!(
        rb(&a, &[b"GET", b"{c74}fresh"]).await,
        "1",
        "sibling committed"
    );
}
