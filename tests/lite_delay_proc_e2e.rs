//! Process-level e2e of the delayed-message machinery against the REAL
//! binary with `lite.delay_sweep_ms` armed: the spawned due sweep (the
//! registered lesson "scanner spawn path MUST have process e2e"), a
//! BLOCK reader woken by the exchange (not by its timeout), and kill -9
//! durability for BOTH stable states -- a staged row survives the crash
//! and exchanges after respawn, and an already-exchanged entry survives
//! with it. The in-round semantics live in lite_delay_e2e.rs.

mod common;

use std::time::{Duration, Instant};

use common::lite::{boot_yaml, frame, resp_full, resp_text};
use common::{wait_resp_ready, TOKEN};

/// Sweep rhythm armed in the node's yaml.
const SWEEP_MS: u64 = 200;
/// Poll budget for a due exchange (debug binary + 200ms rhythm + slow
/// CI runners; the nominal latency is delay + ~1 tick).
const POLL: Duration = Duration::from_secs(15);

fn yaml() -> String {
    format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n")
}

/// XLEN of one stream (0 on any parse surprise).
async fn xlen(a: &str, s: &[u8]) -> u64 {
    resp_text(a, &[b"xlen", s])
        .await
        .trim_start_matches(':')
        .trim_end()
        .parse()
        .unwrap_or(0)
}

/// XRANGE full text (arrays-of-arrays need the full-drain reader).
async fn xrange_all(a: &str, s: &[u8]) -> String {
    resp_full(a, &[b"xrange", s, b"-", b"+"]).await
}

/// Poll XLEN until it reaches `want` (context-tagged panic on budget
/// end) -- the observable of "the spawned scanner exchanged rows".
async fn until_xlen(a: &str, s: &[u8], want: u64, what: &str) {
    let start = Instant::now();
    loop {
        let got = xlen(a, s).await;
        if got == want {
            return;
        }
        assert!(
            start.elapsed() < POLL,
            "{what}: xlen stuck at {got} (want {want})"
        );
        tokio::time::sleep(Duration::from_millis(SWEEP_MS / 2)).await;
    }
}

#[tokio::test]
async fn spawned_sweep_exchanges_when_due() {
    let (_node, a) = boot_yaml("44201", &yaml()).await;
    // Not visible before due even with the sweep running...
    let id = resp_text(
        &a,
        &[
            b"xadd",
            b"orders/q0",
            b"*",
            b"DELAY",
            b"1500",
            b"evt",
            b"timeout",
        ],
    )
    .await;
    assert!(id.starts_with('$'), "reserved id reply: {id}");
    assert_eq!(xlen(&a, b"orders/q0").await, 0, "staged, not an entry");
    // ...then exchanged by the SPAWNED scanner with no client poking it.
    until_xlen(&a, b"orders/q0", 1, "spawned sweep exchange").await;
    let range = xrange_all(&a, b"orders/q0").await;
    assert!(range.contains("timeout"), "{range}");
    // And it is a plain entry afterwards: a group consumes and acks it.
    resp_text(&a, &[b"xgroup", b"create", b"orders/q0", b"g", b"0-0"]).await;
    let read = resp_full(
        &a,
        &[
            b"xreadgroup",
            b"group",
            b"g",
            b"c1",
            b"streams",
            b"orders/q0",
            b">",
        ],
    )
    .await;
    assert!(read.contains("timeout"), "{read}");
}

#[tokio::test]
async fn blocked_reader_wakes_on_the_spawned_exchange() {
    let (_node, a) = boot_yaml("44202", &yaml()).await;
    resp_text(&a, &[b"xadd", b"park/q0", b"1-1", b"seed", b"0"]).await;
    // Persistent connection parks on XREAD BLOCK (pipelined AUTH+XREAD,
    // the same shape as lite_e2e's BLOCK test); `$` resolves to the
    // reserved id of the staged row, so only the exchange can satisfy it
    // before the 12s budget -- the wake must come from the sweep's notify.
    let mut sock = tokio::net::TcpStream::connect(&a).await.expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut pipelined = frame(&[b"AUTH", TOKEN.as_bytes()]);
    pipelined.extend_from_slice(&frame(&[
        b"XREAD", b"BLOCK", b"12000", b"STREAMS", b"park/q0", b"$",
    ]));
    sock.write_all(&pipelined).await.expect("pipeline write");
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(2), sock.read_exact(&mut hello))
        .await
        .expect("auth reply before park")
        .expect("read");
    assert_eq!(&hello, b"+OK\r\n");
    resp_text(
        &a,
        &[b"xadd", b"park/q0", b"*", b"DELAY", b"900", b"wake", b"now"],
    )
    .await;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 1024];
    tokio::time::timeout(POLL, async {
        loop {
            let n = sock.read(&mut chunk).await.expect("reader socket");
            reply.extend_from_slice(&chunk[..n]);
            if String::from_utf8_lossy(&reply).contains("now") {
                break;
            }
        }
    })
    .await
    .expect("reader woke well inside its BLOCK budget");
    assert!(String::from_utf8_lossy(&reply).contains("now"), "{reply:?}");
}

#[tokio::test]
async fn kill9_keeps_staged_rows_and_exchanged_entries() {
    let (mut node, a) = boot_yaml("44203", &yaml()).await;
    // One row stages past the crash point, one exchanges before it.
    let late = resp_text(
        &a,
        &[b"xadd", b"d/q0", b"*", b"DELAY", b"6000", b"evt", b"later"],
    )
    .await;
    let early = resp_text(
        &a,
        &[b"xadd", b"d/q0", b"*", b"DELAY", b"700", b"evt", b"early"],
    )
    .await;
    assert!(late.starts_with('$') && early.starts_with('$'));
    until_xlen(&a, b"d/q0", 1, "early row exchanged pre-crash").await;
    // kill -9 (SIGKILL: no graceful flush anywhere) + respawn on the
    // same data dir (same yaml: the sweep re-arms itself).
    node.kill_now();
    node.respawn();
    wait_resp_ready(&mut node, 30).await;
    let a2 = node.resp.clone();
    until_xlen(
        &a2,
        b"d/q0",
        2,
        "exchanged entry survived + staged row exchanged post-crash",
    )
    .await;
    let range = xrange_all(&a2, b"d/q0").await;
    assert!(
        range.contains("early") && range.contains("later"),
        "{range}"
    );
    let (e, l) = (range.find("early"), range.find("later"));
    assert!(e < l, "due order read order: {range}");
}
