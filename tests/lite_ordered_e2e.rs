//! Ordered consumer groups + contiguous-prefix commit, calibrated
//! against Kafka's ordering model (see `features/mq-lite.md`):
//!
//! - P0 queue-exclusive ownership: one consumer per ordered group's
//!   queue; idle lease migration, no coordinator; strict-serial by
//!   default with an INFLIGHT prefetch knob.
//! - P1 committed watermark = Kafka committed offset: advances only
//!   over a CONTIGUOUS acked prefix; a restart redelivers the
//!   uncommitted tail (duplicates, never loss).
//! - P2 claim takeover is queue-granular: only the PEL head transfers
//!   ownership (epoch fencing deposes the old owner's `>` reads).

mod common;

use common::lite::{call, open_shared, pel_rows, shared_at, text};
use rdb::conf;
use rdb::state;

fn read_new(shared: &state::Shared, group: &[u8], consumer: &[u8]) -> Vec<u8> {
    call(
        shared,
        "xreadgroup",
        &[b"group", group, consumer, b"streams", b"o/q0", b">"],
    )
}

#[test]
fn ordered_strict_serial_default_and_inflight_knob() {
    let (shared, _dir) = shared_at("44310");
    for i in 1..=5u8 {
        call(&shared, "xadd", &[b"o/q0", b"*", b"f", &[b'v', b'0' + i]]);
    }
    // Default ORDERED: inflight 1 = strict serial.
    assert_eq!(
        call(
            &shared,
            "xgroup",
            &[b"create", b"o/q0", b"g", b"0-0", b"ordered"]
        ),
        b"+OK\r\n".to_vec()
    );
    let r = text(&read_new(&shared, b"g", b"c1"));
    assert!(r.contains("v1") && !r.contains("v2"), "one entry only: {r}");
    // Window full: no ack, no next entry -- for anyone (exclusive).
    assert_eq!(read_new(&shared, b"g", b"c1"), b"*-1\r\n".to_vec());
    assert_eq!(read_new(&shared, b"g", b"c2"), b"*-1\r\n".to_vec());
    // Ack frees the window: the next entry flows, in order, to the owner.
    let id1 = id_of(&r);
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", id1.as_bytes()]),
        b":1\r\n".to_vec()
    );
    let r2 = text(&read_new(&shared, b"g", b"c1"));
    assert!(r2.contains("v2") && !r2.contains("v3"), "{r2}");

    // INFLIGHT 3 = Kafka-style prefetch pipeline of three.
    assert_eq!(
        call(
            &shared,
            "xgroup",
            &[
                b"create",
                b"o/q0",
                b"h",
                b"0-0",
                b"ordered",
                b"inflight",
                b"3"
            ]
        ),
        b"+OK\r\n".to_vec()
    );
    let rh = text(&read_new(&shared, b"h", b"c9"));
    assert!(
        rh.contains("v1") && rh.contains("v3") && !rh.contains("v4"),
        "{rh}"
    );
    // Window of 3 is full with nothing acked.
    assert_eq!(read_new(&shared, b"h", b"c9"), b"*-1\r\n".to_vec());
}

#[test]
fn ordered_ownership_is_exclusive_and_migrates_when_idle() {
    let (shared, _dir) = shared_at("44311");
    for i in 1..=4u8 {
        call(&shared, "xadd", &[b"o/q0", b"*", b"f", &[b'v', b'0' + i]]);
    }
    call(
        &shared,
        "xgroup",
        &[b"create", b"o/q0", b"g", b"0-0", b"ordered"],
    );
    let r1 = text(&read_new(&shared, b"g", b"c1"));
    assert!(r1.contains("v1"), "{r1}");
    // Fresh lease: the queue is c1's; c2 is fenced out (nil, not a steal).
    assert_eq!(read_new(&shared, b"g", b"c2"), b"*-1\r\n".to_vec());
    // A FULL window does not migrate via `>` -- stuck work moves via
    // XCLAIM/XAUTOCLAIM. Ack frees the slot first.
    let ids: Vec<String> = all_ids(&shared);
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", ids[0].as_bytes()]),
        b":1\r\n".to_vec()
    );
    // Lease expiry hands the queue to the next asker (idle migration).
    shared.lite.set_lease_ms(1);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let r2 = text(&read_new(&shared, b"g", b"c2"));
    assert!(r2.contains("v2"), "c2 took over: {r2}");
    // Restore a healthy lease so c2's fresh grant is not itself ripe
    // for another takeover while we probe the fencing.
    shared.lite.set_lease_ms(30_000);
    // The deposed c1 is fenced: no NEW entries for the zombie, even
    // after c2 acks and a slot opens up again.
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", ids[1].as_bytes()]),
        b":1\r\n".to_vec()
    );
    assert_eq!(read_new(&shared, b"g", b"c1"), b"*-1\r\n".to_vec());
    assert!(text(&read_new(&shared, b"g", b"c2")).contains("v3"));
    // XINFO GROUPS exposes the live owner + epoch.
    let info = text(&call(&shared, "xinfo", &[b"groups", b"o/q0"]));
    assert!(
        info.contains("ordered") && info.contains("inflight"),
        "{info}"
    );
    assert!(info.contains("c2"), "owner visible: {info}");
}

#[test]
fn ordered_claim_takes_over_only_from_the_pel_head() {
    let (shared, _dir) = shared_at("44312");
    for i in 1..=4u8 {
        call(&shared, "xadd", &[b"o/q0", b"*", b"f", &[b'v', b'0' + i]]);
    }
    call(
        &shared,
        "xgroup",
        &[
            b"create",
            b"o/q0",
            b"g",
            b"0-0",
            b"ordered",
            b"inflight",
            b"2",
        ],
    );
    let r1 = text(&read_new(&shared, b"g", b"c1"));
    assert!(r1.contains("v1") && r1.contains("v2"), "{r1}");
    let ids = all_ids(&shared);
    // Claiming the SECOND pending id (not the head): silently ignored.
    let deep = text(&call(
        &shared,
        "xclaim",
        &[b"o/q0", b"g", b"c2", b"0", ids[1].as_bytes()],
    ));
    assert!(deep == "*0\r\n", "non-head claim ignored: {deep}");
    // Ownership did NOT flip on the failed claim.
    assert!(text(&read_new(&shared, b"g", b"c2")) == "*-1\r\n");
    // Head claim succeeds and takes the queue (min-idle 0).
    let head = text(&call(
        &shared,
        "xclaim",
        &[b"o/q0", b"g", b"c2", b"0", ids[0].as_bytes()],
    ));
    assert!(head.contains("v1"), "head claimed: {head}");
    // The deposed owner is fenced; the new owner drains in order.
    assert_eq!(read_new(&shared, b"g", b"c1"), b"*-1\r\n".to_vec());
    for id in &ids[..2] {
        call(&shared, "xack", &[b"o/q0", b"g", id.as_bytes()]);
    }
    let r3 = text(&read_new(&shared, b"g", b"c2"));
    assert!(r3.contains("v3") && r3.contains("v4"), "{r3}");
}

#[test]
fn ordered_autoclaim_claims_only_the_head() {
    let (shared, _dir) = shared_at("44313");
    for i in 1..=3u8 {
        call(&shared, "xadd", &[b"o/q0", b"*", b"f", &[b'v', b'0' + i]]);
    }
    call(
        &shared,
        "xgroup",
        &[
            b"create",
            b"o/q0",
            b"g",
            b"0-0",
            b"ordered",
            b"inflight",
            b"2",
        ],
    );
    let r1 = text(&read_new(&shared, b"g", b"c1"));
    assert!(r1.contains("v1") && r1.contains("v2"), "{r1}");
    // COUNT 2 asks for two, but only the head may change hands.
    let ac = text(&call(
        &shared,
        "xautoclaim",
        &[b"o/q0", b"g", b"c2", b"0", b"0-0", b"count", b"2"],
    ));
    assert!(ac.contains("v1") && !ac.contains("v2"), "head only: {ac}");
    // c2 owns the queue now: c1 is fenced out of NEW entries.
    let ids = all_ids(&shared);
    call(&shared, "xack", &[b"o/q0", b"g", ids[0].as_bytes()]);
    // Ack frees one slot: the new owner gets the next NEW entry (v3),
    // while v2 (still pending, owner's to re-claim) is not re-served.
    assert!(text(&read_new(&shared, b"g", b"c2")).contains("v3"));
    assert_eq!(read_new(&shared, b"g", b"c1"), b"*-1\r\n".to_vec());
}

#[test]
fn committed_watermark_is_a_contiguous_prefix() {
    let (shared, path) = shared_at("44314");
    for i in 1..=3u8 {
        call(
            &shared,
            "xadd",
            &[
                b"o/q0",
                format!("1-{i}").as_bytes(),
                b"f",
                &[b'v', b'0' + i],
            ],
        );
    }
    call(&shared, "xgroup", &[b"create", b"o/q0", b"g", b"0-0"]);
    let r = text(&read_new(&shared, b"g", b"c1"));
    assert!(r.contains("v1") && r.contains("v3"), "{r}");
    // Out-of-order ack of the LAST id only: it counts (reply 1) but the
    // committed position must NOT skip the pending 1-1/1-2.
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", b"1-3"]),
        b":1\r\n".to_vec()
    );
    let info = text(&call(&shared, "xinfo", &[b"groups", b"o/q0"]));
    assert!(
        info.contains("committed-id") && info.contains("0-0"),
        "position frozen at 0-0: {info}"
    );
    // Close the gap: the prefix advances to 1-2.
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    let info = text(&call(&shared, "xinfo", &[b"groups", b"o/q0"]));
    assert!(info.contains("1-2"), "prefix committed: {info}");
    // Restart: delivery resumes from committed -- 1-3 is REDELIVERED
    // (duplicate) even though it was acked before the gap closed. The
    // old skip-commit semantics would have resumed past it (loss).
    drop(shared);
    let conf = conf::Config {
        bind: "127.0.0.1:44314".to_string(),
        ..Default::default()
    };
    let shared = open_shared(&conf, &path);
    let r = text(&read_new(&shared, b"g", b"c1"));
    assert!(
        r.contains("v3") && !r.contains("v1"),
        "tail redelivered: {r}"
    );
}

#[test]
fn ordered_config_survives_a_restart() {
    let (shared, path) = shared_at("44315");
    call(&shared, "xadd", &[b"o/q0", b"1-1", b"f", b"v1"]);
    call(
        &shared,
        "xgroup",
        &[
            b"create",
            b"o/q0",
            b"g",
            b"0-0",
            b"ordered",
            b"inflight",
            b"2",
        ],
    );
    let r = text(&read_new(&shared, b"g", b"c1"));
    assert!(r.contains("v1"), "{r}");
    drop(shared);
    let conf = conf::Config {
        bind: "127.0.0.1:44315".to_string(),
        ..Default::default()
    };
    let shared = open_shared(&conf, &path);
    call(&shared, "xadd", &[b"o/q0", b"1-2", b"f", b"v2"]);
    // Restart reloaded the group: still ordered, window still holds the
    // unacked 1-1 (redelivered on resume) until it is acked.
    let r = text(&read_new(&shared, b"g", b"c2"));
    assert!(
        r.contains("v1") && !r.contains("v2"),
        "head redelivered: {r}"
    );
    // The redelivery re-uses the existing PEL row, so one of the two
    // window slots is still free: the next read admits 1-2, then the
    // window is full (nil) until something is acked.
    assert!(text(&read_new(&shared, b"g", b"c2")).contains("v2"));
    assert!(text(&read_new(&shared, b"g", b"c2")) == "*-1\r\n");
}

#[test]
fn ordered_setid_rewind_replays_under_ownership() {
    let (shared, path) = shared_at("44316");
    for i in 1..=3u8 {
        call(
            &shared,
            "xadd",
            &[
                b"o/q0",
                format!("1-{i}").as_bytes(),
                b"f",
                &[b'v', b'0' + i],
            ],
        );
    }
    // INFLIGHT 2 + COUNT 1 reads: one entry per read while the second
    // window slot stays free. SETID rewinds the watermarks but frees
    // NO window (only an ack does), so with strict serial (inflight 1)
    // the full window would keep the owner parked out and the rewind
    // could never replay anything.
    call(
        &shared,
        "xgroup",
        &[
            b"create",
            b"o/q0",
            b"g",
            b"0-0",
            b"ordered",
            b"inflight",
            b"2",
        ],
    );
    let r = text(&read_one(&shared, b"g", b"c1"));
    assert!(r.contains("v1") && !r.contains("v2"), "{r}");
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", b"1-1"]),
        b":1\r\n".to_vec()
    );
    let r = text(&read_one(&shared, b"g", b"c1"));
    assert!(r.contains("v2") && !r.contains("v3"), "{r}");
    // SETID rewinds BOTH watermarks; the PEL and the queue ownership
    // are untouched.
    assert_eq!(
        call(&shared, "xgroup", &[b"setid", b"o/q0", b"g", b"1-1"]),
        b"+OK\r\n".to_vec()
    );
    let info = text(&call(&shared, "xinfo", &[b"groups", b"o/q0"]));
    assert!(
        info.contains("1-1") && info.contains("ordered"),
        "watermarks rewound: {info}"
    );
    // Ownership NOT released by the rewind: c2 stays fenced out.
    assert_eq!(read_one(&shared, b"g", b"c2"), b"*-1\r\n".to_vec());
    // The owner replays from 1-1: the already-pending 1-2 row is
    // re-OWNED (not re-counted) with its delivery count carried over
    // and bumped.
    let r = text(&read_one(&shared, b"g", b"c1"));
    assert!(
        r.contains("v2") && !r.contains("v3"),
        "replay 1-2 only: {r}"
    );
    let rows = pel_rows(&call(
        &shared,
        "xpending",
        &[b"o/q0", b"g", b"-", b"+", b"10"],
    ));
    let row12 = rows
        .iter()
        .find(|row| row[1] == "1-2")
        .expect("1-2 still pending");
    assert_eq!(row12[5], ":2", "times_delivered carried over + bumped");
    // Ack the contiguous tail: the committed prefix advances to 1-3
    // (1-3 was never delivered, but it sits beyond the watermark with
    // no surviving pending row in between).
    assert_eq!(
        call(&shared, "xack", &[b"o/q0", b"g", b"1-2", b"1-3"]),
        b":2\r\n".to_vec()
    );
    let info = text(&call(&shared, "xinfo", &[b"groups", b"o/q0"]));
    assert!(info.contains("1-3"), "prefix committed: {info}");
    // SETID persists the rewound group record in its own synchronous
    // commit (and the ack above persisted the advanced watermark), so
    // the restart assertion needs no flusher sleep: delivery resumes
    // at committed 1-3 and the group is still ordered + exclusive.
    drop(shared);
    let conf = conf::Config {
        bind: "127.0.0.1:44316".to_string(),
        ..Default::default()
    };
    let shared = open_shared(&conf, &path);
    assert_eq!(read_one(&shared, b"g", b"c1"), b"*-1\r\n".to_vec());
    assert_eq!(read_one(&shared, b"g", b"c2"), b"*-1\r\n".to_vec());
    call(&shared, "xadd", &[b"o/q0", b"2-1", b"f", b"v4"]);
    assert!(text(&read_one(&shared, b"g", b"c1")).contains("v4"));
    assert_eq!(read_one(&shared, b"g", b"c2"), b"*-1\r\n".to_vec());
}

// ---- helpers -------------------------------------------------------------

/// `>` read capped at COUNT 1: one delivery per call.
fn read_one(shared: &state::Shared, group: &[u8], consumer: &[u8]) -> Vec<u8> {
    call(
        shared,
        "xreadgroup",
        &[
            b"group", group, consumer, b"count", b"1", b"streams", b"o/q0", b">",
        ],
    )
}

fn id_of(reply_text: &str) -> String {
    reply_text
        .split(&['\r', '\n'][..])
        .find(|t| t.contains('-') && t.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .expect("an id in the reply")
        .to_string()
}

fn all_ids(shared: &state::Shared) -> Vec<String> {
    let r = text(&call(shared, "xrange", &[b"o/q0", b"-", b"+"]));
    r.split(&['\r', '\n'][..])
        .filter(|t| t.contains('-') && t.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .map(|t| t.to_string())
        .collect()
}
