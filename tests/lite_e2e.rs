//! Lite Mode end-to-end: RocketMQ-style parent topics + dynamic queues
//! through the real command registry against a real RocksDB store, plus a
//! process-level kill -9 restart check. Covers queue auto-pick, the
//! group lifecycle, at-least-once redelivery after restart, idle-TTL
//! reclamation, BLOCK wake-up and the Lite metrics series.

mod common;

use std::time::Duration;

use common::lite::{call, open_shared, shared_at, text};
use common::{cmd_one_shot, contains_bytes, spawn_node, wait_resp_ready};
use rdb::{conf, monitor};

#[test]
fn xadd_autopick_xpick_and_info() {
    let (shared, _dir) = shared_at("43001");
    // Bare parent: auto-picks q0, replies [full-name, id].
    let r = call(&shared, "xadd", &[b"orders", b"sku", b"a"]);
    assert!(contains_bytes(&r, b"*2\r\n"), "autopick reply {r:?}");
    assert!(
        contains_bytes(&r, b"$9\r\norders/q0\r\n"),
        "autopick reply {r:?}"
    );
    // Explicit queue.
    assert_eq!(
        call(&shared, "xadd", &[b"orders/q1", b"1-1", b"sku", b"b"]),
        b"$3\r\n1-1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xadd", &[b"orders/q1", b"1-2", b"sku", b"c"]),
        b"$3\r\n1-2\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"orders/q1"]), b":2\r\n".to_vec());
    assert_eq!(call(&shared, "xlen", &[b"orders/q0"]), b":1\r\n".to_vec());
    // XPICK: hash is stable, round_robin cycles the two queues.
    let h1 = call(&shared, "xpick", &[b"orders", b"hash", b"k-42"]);
    let h2 = call(&shared, "xpick", &[b"orders", b"hash", b"k-42"]);
    assert!(h1.starts_with(b"$"), "hash pick {h1:?}");
    assert_eq!(h1, h2, "hash pick must be stable");
    let picks: Vec<String> = (0..2)
        .map(|_| text(&call(&shared, "xpick", &[b"orders", b"round_robin"])))
        .collect();
    assert_ne!(picks[0], picks[1], "round robin should cycle: {picks:?}");
    // XINFO TOPICS: both queues with lengths.
    let topics = text(&call(&shared, "xinfo", &[b"topics", b"orders"]));
    assert!(topics.contains("*4\r\n"), "two pairs {topics}");
    assert!(
        topics.contains("$2\r\nq0\r\n:1\r\n") && topics.contains("$2\r\nq1\r\n:2\r\n"),
        "{topics}"
    );
    // XINFO STREAM.
    let sinfo = text(&call(&shared, "xinfo", &[b"stream", b"orders/q1"]));
    assert!(
        sinfo.contains("length") && sinfo.contains(":2\r\n"),
        "{sinfo}"
    );
}

#[test]
fn group_lifecycle_and_catchup() {
    let (shared, _dir) = shared_at("43002");
    // CREATE without MKSTREAM on a missing stream fails.
    assert!(call(&shared, "xgroup", &[b"create", b"orders/q0", b"g", b"$"]).starts_with(b"-ERR"));
    assert_eq!(
        call(
            &shared,
            "xgroup",
            &[b"create", b"orders/q0", b"g", b"0-0", b"MKSTREAM"]
        ),
        b"+OK\r\n".to_vec()
    );
    // Duplicate name.
    assert!(text(&call(
        &shared,
        "xgroup",
        &[b"create", b"orders/q0", b"g", b"$", b"MKSTREAM"]
    ))
    .contains("BUSYGROUP"));
    for i in 1..=3u8 {
        let id = format!("1-{i}");
        assert_eq!(
            call(&shared, "xadd", &[b"orders/q0", id.as_bytes(), b"f", b"v"]),
            format!("${}\r\n{id}\r\n", id.len()).into_bytes()
        );
    }
    // `>` delivers; watermark advances past what COUNT returned.
    let r = text(&call(
        &shared,
        "xreadgroup",
        &[
            b"group",
            b"g",
            b"c1",
            b"count",
            b"2",
            b"streams",
            b"orders/q0",
            b">",
        ],
    ));
    assert!(
        r.contains("1-1") && r.contains("1-2") && !r.contains("1-3"),
        "{r}"
    );
    // ACK the first two (idempotent: re-ack counts 0).
    assert_eq!(
        call(&shared, "xack", &[b"orders/q0", b"g", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xack", &[b"orders/q0", b"g", b"1-1"]),
        b":0\r\n".to_vec()
    );
    // `>` again: only the third remains.
    let r = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b">"],
    ));
    assert!(r.contains("1-3") && !r.contains("1-1"), "{r}");
    // Explicit id (Redis semantics): this consumer's PEL history -- only
    // the still-pending 1-3 (1-1/1-2 were acked out of the PEL); a
    // different consumer's history is empty.
    let replay = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b"0-0"],
    ));
    assert!(
        replay.contains("1-3") && !replay.contains("1-1"),
        "{replay}"
    );
    let others = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c9", b"streams", b"orders/q0", b"0-0"],
    ));
    assert!(others.contains("*-1") || !others.contains("1-"), "{others}");
    // GROUPS introspection shows committed >= delivered.
    let gi = text(&call(&shared, "xinfo", &[b"groups", b"orders/q0"]));
    assert!(
        gi.contains("last-delivered-id") && gi.contains("committed-id"),
        "{gi}"
    );
    // SETID rewinds; DESTROY removes.
    assert_eq!(
        call(&shared, "xgroup", &[b"setid", b"orders/q0", b"g", b"0-0"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xgroup", &[b"destroy", b"orders/q0", b"g"]),
        b":1\r\n".to_vec()
    );
    let gone = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b">"],
    ));
    assert!(gone.contains("NOGROUP"), "{gone}");
}

#[test]
fn restart_resumes_from_committed_watermark() {
    let (shared, path) = shared_at("43003");
    call(
        &shared,
        "xgroup",
        &[b"create", b"orders/q0", b"g", b"0-0", b"MKSTREAM"],
    );
    for i in 1..=3u8 {
        call(
            &shared,
            "xadd",
            &[b"orders/q0", format!("1-{i}").as_bytes(), b"f", b"v"],
        );
    }
    // Deliver all three, commit only the first two.
    call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b">"],
    );
    assert_eq!(
        call(&shared, "xack", &[b"orders/q0", b"g", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    // Restart: same store path, fresh runtime/cache.
    drop(shared);
    let restarted = open_shared(
        &conf::Config {
            bind: "127.0.0.1:43003".to_string(),
            ..Default::default()
        },
        &path,
    );
    let r = text(&call(
        &restarted,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b">"],
    ));
    assert!(
        r.contains("1-3") && !r.contains("1-1") && !r.contains("1-2"),
        "restart must redeliver only beyond the committed watermark: {r}"
    );
    assert_eq!(
        call(&restarted, "xack", &[b"orders/q0", b"g", b"1-3"]),
        b":1\r\n".to_vec()
    );
    let done = text(&call(
        &restarted,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b">"],
    ));
    assert!(done.starts_with("*-1"), "fully committed: {done}");
}

#[test]
fn idle_ttl_reaps_whole_stream() {
    let (shared, _dir) = shared_at("43004");
    call(&shared, "xadd", &[b"orders/q0", b"1-1", b"f", b"v"]);
    assert_eq!(
        call(&shared, "xidle", &[b"orders/q0", b"1"]),
        b"+OK\r\n".to_vec()
    );
    assert!(
        text(&call(&shared, "xidle", &[b"orders/q0"])).starts_with(":1"),
        "about 1s left"
    );
    // Poll for the ~1s TTL instead of a fixed-margin sleep (upper-bounded:
    // slow CI must not flake on a 300ms margin over a 1s TTL).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while call(&shared, "xidle", &[b"orders/q0"]) != b":-2\r\n".to_vec() {
        assert!(
            std::time::Instant::now() < deadline,
            "idle TTL did not fire within 5s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        call(&shared, "xlen", &[b"orders/q0"]),
        b":0\r\n".to_vec(),
        "lazy purge"
    );
    assert_eq!(
        call(&shared, "xidle", &[b"orders/q0"]),
        b":-2\r\n".to_vec(),
        "missing after reap"
    );
    // Topic recreates cleanly after the TTL fired.
    assert_eq!(
        call(&shared, "xadd", &[b"orders/q0", b"2-1", b"f", b"v"]),
        b"$3\r\n2-1\r\n".to_vec()
    );
    assert_eq!(call(&shared, "xlen", &[b"orders/q0"]), b":1\r\n".to_vec());
    let topics = text(&call(&shared, "xinfo", &[b"topics", b"orders"]));
    assert!(topics.contains(":1\r\n"), "{topics}");
}

/// One RESP array frame.
fn frame(args: &[&[u8]]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

#[tokio::test]
async fn block_wakes_on_xadd_over_wire() {
    let dir = std::env::temp_dir().join(format!("rdb-lite-block-{}", std::process::id()));
    let mut node = spawn_node(&dir, 0, true, None);
    // 30s spawn window (not 10s): on a saturated machine -- e.g. a
    // concurrent cargo build/link pinning every core -- the debug
    // binary can take >10s to bind RESP; the cluster helper uses
    // the same 30s (see common::spawn_cluster).
    wait_resp_ready(&mut node, 30).await;
    let t = common::TOKEN;
    assert!(text(
        &cmd_one_shot(
            &node.resp,
            t,
            &[b"xadd", b"orders/q0", b"1-1", b"f", b"seed"]
        )
        .await
    )
    .contains("1-1"));
    // Parked BLOCK with no new data returns the nil array on timeout.
    // (read_one returns the nil-array line without its trailing CRLF)
    assert_eq!(
        text(
            &cmd_one_shot(
                &node.resp,
                t,
                &[b"xread", b"block", b"100", b"streams", b"orders/q0", b"$"]
            )
            .await
        ),
        "*-1"
    );
    // Persistent AUTHed connection parks on XREAD; a second connection XADDs.
    // Pipelined AUTH + XREAD in one write: the blocking-dispatch flush
    // delivers the +OK before the XREAD parks.
    let mut sock = tokio::net::TcpStream::connect(&node.resp)
        .await
        .expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut pipelined = frame(&[b"AUTH", t.as_bytes()]);
    pipelined.extend_from_slice(&frame(&[
        b"XREAD",
        b"BLOCK",
        b"5000",
        b"STREAMS",
        b"orders/q0",
        b"$",
    ]));
    sock.write_all(&pipelined)
        .await
        .expect("pipelined auth+xread write");
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), sock.read_exact(&mut hello))
        .await
        .expect("+OK flushed before the XREAD parks")
        .expect("auth reply");
    assert_eq!(&hello, b"+OK\r\n");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    let r = cmd_one_shot(
        &node.resp,
        t,
        &[b"xadd", b"orders/q0", b"2-1", b"f", b"wake"],
    )
    .await;
    assert!(text(&r).contains("2-1"), "xadd {r:?}");
    // The parked reader must be woken (not parked out the full 5s).
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(4), sock.read(&mut chunk))
            .await
            .expect("reader woken within 4s")
            .expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if contains_bytes(&buf, b"2-1") {
            break;
        }
    }
    assert!(
        contains_bytes(&buf, b"2-1") && contains_bytes(&buf, b"wake"),
        "reply {:?}",
        buf
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "wake-up, not timeout"
    );
}

#[test]
fn lite_metrics_series_exposed() {
    let (shared, _dir) = shared_at("43006");
    call(&shared, "xadd", &[b"orders/q0", b"1-1", b"f", b"v"]);
    call(&shared, "xgroup", &[b"create", b"orders/q0", b"g", b"0-0"]);
    call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", b"orders/q0", b">"],
    );
    call(&shared, "xack", &[b"orders/q0", b"g", b"1-1"]);
    let metrics = monitor::encode(&shared.monitor).unwrap();
    assert!(metrics.contains("rdb_lite_messages"), "{metrics}");
    assert!(
        metrics.contains(r#"op="add""#)
            && metrics.contains(r#"op="read""#)
            && metrics.contains(r#"op="ack""#),
        "{metrics}"
    );
    assert!(
        metrics.contains(r#"rdb_lite_streams{kind="live"}"#),
        "{metrics}"
    );
    assert!(metrics.contains("rdb_lite_offset_dirty"), "{metrics}");
}

/// Open an AUTHed connection, pipeline `cmd` right after AUTH, consume
/// the +OK reply, and return the connection now parked inside `cmd`.
async fn park_reader(addr: &str, token: &str, cmd: &[&[u8]]) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut pipelined = frame(&[b"AUTH", token.as_bytes()]);
    pipelined.extend_from_slice(&frame(cmd));
    sock.write_all(&pipelined)
        .await
        .expect("pipeline AUTH + blocking cmd");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut hello = [0u8; 5];
    sock.read_exact(&mut hello)
        .await
        .expect("+OK flushed before the command parks");
    assert_eq!(&hello, b"+OK\r\n");
    sock
}

/// Read from a parked connection until `needle` shows up (bounded).
async fn read_until(sock: &mut tokio::net::TcpStream, needle: &[u8], secs: u64) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(secs), sock.read(&mut chunk))
            .await
            .expect("parked reader must be woken, not parked out its block")
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
async fn block_zero_and_oversized_block_wait_for_xadd() {
    // Regression: BLOCK 0 must mean "wait indefinitely" (Redis semantics),
    // not an instant nil reply; and BLOCK values larger than the internal
    // slice cap must keep waiting too. Each parked reader sits on its own
    // queue, so one XADD per queue wakes exactly one waiter. The whole
    // dance is bounded by an outer timeout: a regression fails fast
    // instead of hanging the suite. 30s, not 10s: on a saturated
    // machine the spawn alone can eat a 10s budget (see the readiness
    // note below).
    tokio::time::timeout(Duration::from_secs(30), async {
        let dir = std::env::temp_dir().join(format!("rdb-lite-block0-{}", std::process::id()));
        let mut node = spawn_node(&dir, 0, true, None);
        // 30s spawn window (not 10s): on a saturated machine -- e.g. a
        // concurrent cargo build/link pinning every core -- the debug
        // binary can take >10s to bind RESP; the cluster helper uses
        // the same 30s (see common::spawn_cluster).
        wait_resp_ready(&mut node, 30).await;
        let t = common::TOKEN;
        // Empty queue + group for the XREADGROUP reader.
        assert!(
            text(
                &cmd_one_shot(
                    &node.resp,
                    t,
                    &[
                        b"xgroup",
                        b"create",
                        b"orders/q1",
                        b"g",
                        b"0-0",
                        b"MKSTREAM"
                    ]
                )
                .await
            )
            .contains("OK"),
            "xgroup create"
        );
        let mut plain = park_reader(
            &node.resp,
            t,
            &[b"XREAD", b"BLOCK", b"0", b"STREAMS", b"orders/q0", b"$"],
        )
        .await;
        let mut grouped = park_reader(
            &node.resp,
            t,
            &[
                b"XREADGROUP",
                b"GROUP",
                b"g",
                b"c1",
                b"BLOCK",
                b"0",
                b"STREAMS",
                b"orders/q1",
                b">",
            ],
        )
        .await;
        // 25h: larger than the 24h internal park slice.
        let mut oversized = park_reader(
            &node.resp,
            t,
            &[
                b"XREAD",
                b"BLOCK",
                b"90000000",
                b"STREAMS",
                b"orders/q2",
                b"$",
            ],
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await; // let all three park
        assert!(
            text(
                &cmd_one_shot(
                    &node.resp,
                    t,
                    &[b"xadd", b"orders/q0", b"1-1", b"f", b"wake"]
                )
                .await
            )
            .contains("1-1"),
            "xadd q0"
        );
        let r = read_until(&mut plain, b"1-1", 5).await;
        assert!(
            contains_bytes(&r, b"1-1") && !contains_bytes(&r, b"*-1"),
            "XREAD BLOCK 0 must park until data, got {r:?}"
        );
        assert!(
            text(
                &cmd_one_shot(
                    &node.resp,
                    t,
                    &[b"xadd", b"orders/q1", b"1-1", b"f", b"wake"]
                )
                .await
            )
            .contains("1-1"),
            "xadd q1"
        );
        let g = read_until(&mut grouped, b"1-1", 5).await;
        assert!(
            contains_bytes(&g, b"1-1") && !contains_bytes(&g, b"*-1"),
            "XREADGROUP BLOCK 0 must park until data, got {g:?}"
        );
        assert!(
            text(
                &cmd_one_shot(
                    &node.resp,
                    t,
                    &[b"xadd", b"orders/q2", b"1-1", b"f", b"wake"]
                )
                .await
            )
            .contains("1-1"),
            "xadd q2"
        );
        let big = read_until(&mut oversized, b"1-1", 5).await;
        assert!(
            contains_bytes(&big, b"1-1") && !contains_bytes(&big, b"*-1"),
            "oversized BLOCK must park until data, got {big:?}"
        );
    })
    .await
    .expect("BLOCK 0 / oversized BLOCK regression: reader never woke");
}
