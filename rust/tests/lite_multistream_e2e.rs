//! Lite Mode multi-stream e2e: XREAD / XREADGROUP across a STREAMS
//! list (empty streams omitted, list-order replies, mixed `>` +
//! explicit-id forms, up-front NOGROUP with no half delivery), plus
//! the durability tail of the PEL cover: restart recovery of pending
//! rows with at-least-once redelivery, and the rdb_lite_backlog gauge
//! series. Kept separate so no test file crosses the 400-line gate.

mod common;

use common::lite::{call, open_shared, shared_at, text};
use rdb::state::Shared;
use rdb::{conf, lite, monitor};

/// XADD with an explicit id (`1-<n>`), asserting the echoed id reply.
fn add(shared: &Shared, stream: &[u8], id: &str, v: u8) {
    assert_eq!(
        call(shared, "xadd", &[stream, id.as_bytes(), b"f", &[b'v', v]]),
        format!("${}\r\n{id}\r\n", id.len()).into_bytes(),
        "xadd {id}"
    );
}

/// XGROUP CREATE ... MKSTREAM from 0-0.
fn group(shared: &Shared, stream: &[u8], g: &[u8]) {
    assert_eq!(
        call(
            shared,
            "xgroup",
            &[b"create", stream, g, b"0-0", b"MKSTREAM"]
        ),
        b"+OK\r\n".to_vec(),
        "xgroup create {stream:?}/{g:?}"
    );
}

#[test]
fn multistream_xread() {
    let (shared, _dir) = shared_at("43025");
    let (s1, s2) = (b"ms/s1", b"ms/s2");
    // Only s2 has data: s1 is omitted entirely, not an empty pair.
    add(&shared, s2, "1-1", b'b');
    assert_eq!(
        call(&shared, "xread", &[b"streams", s1, s2, b"0-0", b"0-0"]),
        b"*1\r\n*2\r\n$5\r\nms/s2\r\n*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$2\r\nvb\r\n"
            .to_vec()
    );
    // Both populated: one reply, in STREAMS-list order (s1 before s2),
    // each stream carrying its own payload.
    add(&shared, s1, "1-1", b'a');
    let t = text(&call(
        &shared,
        "xread",
        &[b"streams", s1, s2, b"0-0", b"0-0"],
    ));
    assert!(t.starts_with("*2\r\n"), "{t}");
    assert!(t.find("ms/s1").unwrap() < t.find("ms/s2").unwrap(), "{t}");
    assert!(t.contains("$2\r\nva") && t.contains("$2\r\nvb"), "{t}");
    // Nothing on either stream: the nil array.
    assert_eq!(
        call(
            &shared,
            "xread",
            &[b"streams", b"ms/x", b"ms/y", b"0-0", b"0-0"]
        ),
        b"*-1\r\n".to_vec()
    );
}

#[test]
fn multistream_xreadgroup() {
    let (shared, _dir) = shared_at("43026");
    let (s1, s2, s3) = (b"ms/s1", b"ms/s2", b"ms/s3");
    group(&shared, s1, b"g");
    group(&shared, s2, b"g");
    add(&shared, s1, "1-1", b'a');
    add(&shared, s2, "1-1", b'b');
    // `>` on both streams: one reply carrying both, in list order.
    let t = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s1, s2, b">", b">"],
    ));
    assert!(t.starts_with("*2\r\n"), "{t}");
    assert!(t.find("ms/s1").unwrap() < t.find("ms/s2").unwrap(), "{t}");
    assert!(t.contains("$2\r\nva") && t.contains("$2\r\nvb"), "{t}");
    // Mixed form: explicit id serves c1's PEL history on s1 while `>`
    // delivers the new entry on s2 -- one command, both sections.
    add(&shared, s2, "1-2", b'c');
    let t = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s1, s2, b"0-0", b">"],
    ));
    assert!(t.find("ms/s1").unwrap() < t.find("ms/s2").unwrap(), "{t}");
    assert!(t.contains("1-1") && t.contains("1-2"), "{t}");
    // NOGROUP when any listed stream lacks the group -- validated
    // up-front, so the streams checked before the miss (s1 has a fresh
    // 1-2 waiting) are NOT half-delivered.
    add(&shared, s1, "1-2", b'd');
    add(&shared, s3, "1-1", b'e'); // stream exists, group missing
    let t = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s1, s3, b">", b">"],
    ));
    assert!(t.contains("NOGROUP") && t.contains("ms/s3"), "{t}");
    assert_eq!(
        call(&shared, "xpending", &[s1, b"g"]),
        b"*5\r\n:1\r\n$3\r\n1-1\r\n$3\r\n1-1\r\n$2\r\nc1\r\n:1\r\n".to_vec(),
        "s1 must not have half-delivered 1-2"
    );
}

#[test]
fn restart_recovers_pel() {
    let (shared, path) = shared_at("43027");
    let s = b"rst/t0";
    for (i, v) in (1..=3).zip(b'a'..=b'c') {
        add(&shared, s, &format!("1-{i}"), v);
    }
    group(&shared, s, b"g");
    let t = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s, b">"],
    ));
    assert!(t.contains("1-3"), "{t}");
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-1"]),
        b":1\r\n".to_vec()
    );
    // Restart on the same store: PEL rows survived with ownership.
    drop(shared);
    let restarted = open_shared(
        &conf::Config {
            bind: "127.0.0.1:43027".to_string(),
            ..Default::default()
        },
        &path,
    );
    assert_eq!(
        call(&restarted, "xpending", &[s, b"g"]),
        b"*5\r\n:2\r\n$3\r\n1-2\r\n$3\r\n1-3\r\n$2\r\nc1\r\n:2\r\n".to_vec()
    );
    // XAUTOCLAIM at min-idle 0 reclaims both survivors for c9.
    let t = text(&call(
        &restarted,
        "xautoclaim",
        &[s, b"g", b"c9", b"0", b"0-0", b"COUNT", b"10"],
    ));
    assert!(t.contains("1-2") && t.contains("1-3"), "{t}");
    // The delivered watermark rewound to committed on reload: `>`
    // re-delivers the two unacked entries (at-least-once).
    let t = text(&call(
        &restarted,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s, b">"],
    ));
    assert!(t.contains("1-2") && t.contains("1-3"), "{t}");
    assert!(!t.contains("1-1"), "acked 1-1 must not re-deliver: {t}");
    // Redelivery preserves delivery history: each surviving entry now
    // shows delivery-count 3 (initial + XAUTOCLAIM + re-delivery).
    let t = text(&call(
        &restarted,
        "xpending",
        &[s, b"g", b"IDLE", b"0", b"-", b"+", b"10"],
    ));
    assert!(t.contains("c1") && !t.contains("c9"), "{t}");
    assert!(t.contains(":3"), "times_delivered must carry over: {t}");
}

#[test]
fn backlog_gauge() {
    let (shared, _dir) = shared_at("43028");
    let s = b"bkl/t0";
    for (i, v) in (1..=4).zip(b'a'..=b'd') {
        add(&shared, s, &format!("1-{i}"), v);
    }
    group(&shared, s, b"g");
    let t = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s, b">"],
    ));
    assert!(t.contains("1-4"), "{t}");
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-1"]),
        b":1\r\n".to_vec()
    );
    // The gauge refreshes on the 200ms background loop, which
    // in-process tests never spawn: the SERIES is registered (zero
    // until the first refresh) and the exact value the loop would
    // publish is total_pending == N-k == 3.
    let metrics = monitor::encode(&shared.monitor).unwrap();
    assert!(metrics.contains("rdb_lite_backlog"), "{metrics}");
    assert_eq!(lite::offset::total_pending(&shared.lite.offsets), 3);
    // One manual loop-body round, mirroring spawn_background.
    monitor::set_lite_backlog(
        &shared.monitor,
        lite::offset::total_pending(&shared.lite.offsets) as f64,
    );
    let metrics = monitor::encode(&shared.monitor).unwrap();
    assert!(metrics.contains("rdb_lite_backlog 3"), "{metrics}");
}
