//! Lite Mode PEL e2e: XPENDING summary/range, XCLAIM ownership moves
//! (idle gate, JUSTID, FORCE), XAUTOCLAIM cursor walking + orphan-PEL
//! reaping after XDEL, XGROUP CREATECONSUMER/DELCONSUMER admin, and
//! XACK removing PEL rows below the committed watermark. All through
//! the real command registry against a real RocksDB store. Kept
//! separate from lite_e2e.rs / lite_group_e2e.rs so no test file
//! crosses the 400-line gate (multi-stream reads, restart and the
//! backlog gauge live in lite_multistream_e2e.rs).

mod common;

use common::lite::{call, pel_rows, shared_at, text};
use rdb::state::Shared;

/// XADD with an explicit id (`1-<n>`), asserting the echoed id reply so
/// every later PEL assertion is byte-deterministic.
fn add(shared: &Shared, stream: &[u8], id: &str) {
    assert_eq!(
        call(shared, "xadd", &[stream, id.as_bytes(), b"f", b"v"]),
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

/// One consumer's `>` delivery; asserts the highest id came through.
fn deliver(shared: &Shared, stream: &[u8], g: &[u8], c: &[u8], last: &str) {
    let t = text(&call(
        shared,
        "xreadgroup",
        &[b"group", g, c, b"streams", stream, b">"],
    ));
    assert!(t.contains(last), "delivery missing {last}: {t}");
}

#[test]
fn pel_lifecycle() {
    let (shared, _dir) = shared_at("43020");
    let s = b"pel/t0";
    for i in 1..=3 {
        add(&shared, s, &format!("1-{i}"));
    }
    group(&shared, s, b"g");
    // `>` hands all three to c1 in one reply.
    let r = call(
        &shared,
        "xreadgroup",
        &[b"group", b"g", b"c1", b"streams", s, b">"],
    );
    assert!(
        r.starts_with(b"*1\r\n*2\r\n$6\r\npel/t0\r\n*3\r\n"),
        "{r:?}"
    );
    let t = text(&r);
    for i in 1..=3 {
        assert!(t.contains(&format!("1-{i}")), "{t}");
    }
    // Summary: count 3, min 1-1, max 1-3, consumer c1 holding all 3.
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*5\r\n:3\r\n$3\r\n1-1\r\n$3\r\n1-3\r\n$2\r\nc1\r\n:3\r\n".to_vec()
    );
    // Range: [id, consumer, idle, deliveries] rows in id order.
    let rows = pel_rows(&call(&shared, "xpending", &[s, b"g", b"-", b"+", b"10"]));
    assert_eq!(rows.len(), 3, "{rows:?}");
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[1], format!("1-{}", i + 1), "{row:?}");
        assert_eq!(row[3], "c1", "{row:?}");
        assert_eq!(row[5], ":1", "first delivery: {row:?}");
        let idle: u64 = row[4].trim_start_matches(':').parse().unwrap();
        assert!(idle < 60_000, "idle out of range: {row:?}");
    }
    // Ack two: exactly one pending row stays.
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*5\r\n:1\r\n$3\r\n1-3\r\n$3\r\n1-3\r\n$2\r\nc1\r\n:1\r\n".to_vec()
    );
    // Unknown-consumer filter: empty; COUNT 0 caps to empty.
    assert_eq!(
        call(&shared, "xpending", &[s, b"g", b"-", b"+", b"10", b"c9"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xpending", &[s, b"g", b"-", b"+", b"0"]),
        b"*0\r\n".to_vec()
    );
}

#[test]
fn claim_moves_ownership() {
    let (shared, _dir) = shared_at("43021");
    let s = b"clm/t0";
    add(&shared, s, "1-1");
    add(&shared, s, "1-2");
    group(&shared, s, b"g");
    deliver(&shared, s, b"g", b"c1", "1-2");
    // Full claim: the entry frame comes back and c2 owns 1-1 now.
    assert_eq!(
        call(&shared, "xclaim", &[s, b"g", b"c2", b"0", b"1-1"]),
        b"*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*7\r\n:2\r\n$3\r\n1-1\r\n$3\r\n1-2\r\n$2\r\nc1\r\n:1\r\n$2\r\nc2\r\n:1\r\n".to_vec()
    );
    // Idle gate: a full minute is never met -> nothing claimed.
    assert_eq!(
        call(&shared, "xclaim", &[s, b"g", b"c9", b"60000", b"1-2"]),
        b"*0\r\n".to_vec()
    );
    // JUSTID: bare ids only, and the delivery count is NOT bumped (1).
    assert_eq!(
        call(
            &shared,
            "xclaim",
            &[s, b"g", b"c3", b"0", b"1-2", b"JUSTID"]
        ),
        b"*1\r\n$3\r\n1-2\r\n".to_vec()
    );
    let rows = pel_rows(&call(&shared, "xpending", &[s, b"g", b"-", b"+", b"10"]));
    assert_eq!(
        (
            rows[1][1].as_str(),
            rows[1][3].as_str(),
            rows[1][5].as_str()
        ),
        ("1-2", "c3", ":1"),
        "JUSTID must keep times_delivered at 1"
    );
    // Full claim of the same row: 1 -> 2.
    assert!(text(&call(&shared, "xclaim", &[s, b"g", b"c4", b"0", b"1-2"])).contains("1-2"));
    let rows = pel_rows(&call(&shared, "xpending", &[s, b"g", b"-", b"+", b"10"]));
    assert_eq!(
        (
            rows[1][1].as_str(),
            rows[1][3].as_str(),
            rows[1][5].as_str()
        ),
        ("1-2", "c4", ":2")
    );
    // FORCE re-mints a PEL row for an id that was already acked.
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-2"]),
        b":1\r\n".to_vec()
    );
    assert!(text(&call(&shared, "xpending", &[s, b"g"])).starts_with("*5\r\n:1\r\n"));
    assert!(text(&call(
        &shared,
        "xclaim",
        &[s, b"g", b"c5", b"0", b"1-2", b"FORCE"]
    ))
    .contains("1-2"));
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*7\r\n:2\r\n$3\r\n1-1\r\n$3\r\n1-2\r\n$2\r\nc2\r\n:1\r\n$2\r\nc5\r\n:1\r\n".to_vec()
    );
}

#[test]
fn autoclaim_cursor_and_deleted() {
    let (shared, _dir) = shared_at("43022");
    let s = b"ac/t0";
    for i in 1..=4 {
        add(&shared, s, &format!("1-{i}"));
    }
    group(&shared, s, b"g");
    deliver(&shared, s, b"g", b"c1", "1-4");
    // COUNT 2: two entries claimed; the cursor is the SUCCESSOR of the
    // last scanned row (1-3) -- the row itself would re-pass a min-idle
    // 0 gate on the next call and livelock duplicate deliveries.
    let r = call(
        &shared,
        "xautoclaim",
        &[s, b"g", b"c2", b"0", b"0-0", b"COUNT", b"2"],
    );
    assert!(r.starts_with(b"*3\r\n$3\r\n1-3\r\n*2\r\n"), "{r:?}");
    assert!(r.ends_with(b"*0\r\n") && text(&r).contains("1-1"), "{r:?}");
    // From that cursor: the rest is claimed and the walk ends (0-0).
    let r = call(
        &shared,
        "xautoclaim",
        &[s, b"g", b"c2", b"0", b"1-3", b"COUNT", b"10"],
    );
    assert!(r.starts_with(b"*3\r\n$3\r\n0-0\r\n"), "{r:?}");
    let t = text(&r);
    assert!(t.contains("1-3") && t.contains("1-4"), "{t}");
    // XDEL the payload of a pending id: autoclaim reaps the orphan PEL
    // row into the 3rd reply slot instead of a payload-less entry.
    assert_eq!(call(&shared, "xdel", &[s, b"1-3"]), b":1\r\n".to_vec());
    let r = call(
        &shared,
        "xautoclaim",
        &[s, b"g", b"c2", b"0", b"0-0", b"COUNT", b"10"],
    );
    let t = text(&r);
    assert!(t.ends_with("*1\r\n$3\r\n1-3\r\n"), "deleted slot: {t}");
    let head = &t[..t.len() - "*1\r\n$3\r\n1-3\r\n".len()];
    assert!(
        !head.contains("1-3"),
        "entries must omit the deleted id: {t}"
    );
    assert!(head.contains("1-4"), "{t}");
    // Idle-gated: a minute claims nothing; cursor 0-0, both arrays empty.
    assert_eq!(
        call(
            &shared,
            "xautoclaim",
            &[s, b"g", b"c2", b"60000", b"0-0", b"COUNT", b"10"],
        ),
        b"*3\r\n$3\r\n0-0\r\n*0\r\n*0\r\n".to_vec()
    );
}

#[test]
fn consumer_admin() {
    let (shared, _dir) = shared_at("43023");
    let s = b"cadm/t0";
    group(&shared, s, b"g"); // MKSTREAM: group without any entry
                             // CREATECONSUMER: :1 on first sight, :0 when already registered.
    assert_eq!(
        call(&shared, "xgroup", &[b"createconsumer", s, b"g", b"c5"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xgroup", &[b"createconsumer", s, b"g", b"c5"]),
        b":0\r\n".to_vec()
    );
    add(&shared, s, "1-1");
    add(&shared, s, "1-2");
    deliver(&shared, s, b"g", b"c5", "1-2");
    // DELCONSUMER: replies the purged pending count and empties the PEL.
    assert_eq!(
        call(&shared, "xgroup", &[b"delconsumer", s, b"g", b"c5"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*3\r\n:0\r\n$-1\r\n$-1\r\n".to_vec()
    );
    // The registry row was dropped with the consumer...
    assert_eq!(
        call(&shared, "xgroup", &[b"createconsumer", s, b"g", b"c5"]),
        b":1\r\n".to_vec()
    );
    // ...and XINFO CONSUMERS exposes per-consumer pending counts.
    let r = text(&call(&shared, "xinfo", &[b"consumers", s, b"g"]));
    assert!(r.contains("name") && r.contains("c5"), "{r}");
}

/// XACK replies watermark ADVANCEMENT (Lite semantics, not rows
/// removed), yet must still drop PEL rows at/below the committed
/// watermark: a FORCE-minted pending row below the committed point is
/// removable by a plain re-ack that replies :0.
#[test]
fn ack_below_watermark_still_removes_pel_row() {
    let (shared, _dir) = shared_at("43024");
    let s = b"abw/t0";
    add(&shared, s, "1-1");
    add(&shared, s, "1-2");
    group(&shared, s, b"g");
    deliver(&shared, s, b"g", b"c1", "1-2");
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*3\r\n:0\r\n$-1\r\n$-1\r\n".to_vec()
    );
    assert!(text(&call(
        &shared,
        "xclaim",
        &[s, b"g", b"c2", b"0", b"1-1", b"FORCE"]
    ))
    .contains("1-1"));
    assert!(
        text(&call(&shared, "xpending", &[s, b"g"])).starts_with("*5\r\n:1\r\n"),
        "FORCE must re-mint the pending row"
    );
    // 1-1 sits below the committed watermark 1-2: the reply is :0 but
    // the PEL row must still be gone.
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-1"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "xpending", &[s, b"g"]),
        b"*3\r\n:0\r\n$-1\r\n$-1\r\n".to_vec()
    );
}
