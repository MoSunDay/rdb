//! XCLAIM delivery-hint options e2e (in-process, real registry + real
//! store): IDLE / TIME / RETRYCOUNT rewrites visible through XPENDING's
//! idle and deliveries columns, a TIME-backdated row becoming
//! min-idle eligible immediately, JUSTID claims carrying the same PEL
//! writes, and the syntax-error matrix. P3 backfill item #7.

mod common;

use common::lite::{add, call, claim, deliver, mk_group, pending_rows4, shared_at, text};
use rdb::state::Shared;

const NOT_INT: &str = "-ERR value is not an integer or out of range";

/// Seed one delivered row: add `1-1`, group `g`, deliver to `c1`.
fn seeded(tag: &str) -> (Shared, &'static [u8]) {
    let (shared, _dir) = shared_at(tag);
    let s: &'static [u8] = b"co/t0";
    add(&shared, s, "1-1");
    mk_group(&shared, s, b"g");
    deliver(&shared, s, b"g", b"c1", "1-1");
    (shared, s)
}

#[test]
fn idle_hint_backdates_the_delivery_clock() {
    let (shared, s) = seeded("45020");
    assert_eq!(
        claim(&shared, s, &[b"g", b"c2", b"0", b"1-1", b"IDLE", b"600000"]),
        "*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv"
    );
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("c2", 2), "{rows:?}");
    // IDLE 600000 backdates the clock: the idle column reads AT LEAST
    // that (wall-clock drift only ever adds).
    assert!(rows[0].2 >= 600_000, "idle {} < 600000", rows[0].2);
}

#[test]
fn time_hint_parks_the_wall_clock() {
    let (shared, s) = seeded("45021");
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 3_600_000; // one hour ago
    let past_arg = past.to_string();
    assert_eq!(
        claim(
            &shared,
            s,
            &[b"g", b"c2", b"0", b"1-1", b"TIME", past_arg.as_bytes()]
        ),
        "*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv"
    );
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(rows[0].1, "c2");
    // idle derives from now - delivered_ms: >= 1h, well under 2h.
    assert!(
        rows[0].2 >= 3_600_000 && rows[0].2 < 7_200_000,
        "idle {} not ~1h",
        rows[0].2
    );
    assert_eq!(rows[0].3, 2, "no RETRYCOUNT: the bump still happens");
}

#[test]
fn retrycount_replaces_the_delivery_counter() {
    let (shared, s) = seeded("45022");
    claim(
        &shared,
        s,
        &[b"g", b"c2", b"0", b"1-1", b"RETRYCOUNT", b"7"],
    );
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("c2", 7), "{rows:?}");
    assert!(rows[0].2 < 60_000, "no IDLE/TIME: clock refreshed");
    // A second hint-less claim bumps FROM the override: 7 -> 8.
    claim(&shared, s, &[b"g", b"c1", b"0", b"1-1"]);
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("c1", 8), "{rows:?}");
}

#[test]
fn time_backdated_row_is_min_idle_eligible_at_once() {
    let (shared, s) = seeded("45023");
    // Fresh delivery is NOT idle enough for a 10-minute gate...
    assert_eq!(claim(&shared, s, &[b"g", b"c2", b"600000", b"1-1"]), "*0");
    // ...a TIME claim an hour back makes it eligible immediately.
    let past = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        - 3_600_000;
    let past_arg = past.to_string();
    claim(
        &shared,
        s,
        &[b"g", b"c2", b"0", b"1-1", b"TIME", past_arg.as_bytes()],
    );
    assert_eq!(
        claim(&shared, s, &[b"g", b"c3", b"600000", b"1-1"]),
        "*1\r\n*2\r\n$3\r\n1-1\r\n*2\r\n$1\r\nf\r\n$1\r\nv",
        "backdated row passes the min-idle gate"
    );
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(rows[0].1, "c3");
}

#[test]
fn justid_claim_carries_the_pel_writes() {
    let (shared, s) = seeded("45024");
    assert_eq!(
        claim(
            &shared,
            s,
            &[
                b"g",
                b"c2",
                b"0",
                b"1-1",
                b"JUSTID",
                b"IDLE",
                b"600000",
                b"RETRYCOUNT",
                b"9"
            ]
        ),
        "*1\r\n$3\r\n1-1"
    );
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(
        (rows[0].1.as_str(), rows[0].3),
        ("c2", 9),
        "JUSTID applies IDLE/TIME/RETRYCOUNT like Redis"
    );
    assert!(rows[0].2 >= 600_000, "idle {} < 600000", rows[0].2);
}

#[test]
fn force_hint_and_flag_orders_interleave() {
    let (shared, s) = seeded("45025");
    add(&shared, s, "1-2");
    // Options in any order after the id args; FORCE mints a row that
    // was never delivered, hints apply to it too.
    assert_eq!(
        claim(
            &shared,
            s,
            &[
                b"g",
                b"c2",
                b"0",
                b"RETRYCOUNT",
                b"5",
                b"1-2",
                b"FORCE",
                b"JUSTID"
            ]
        ),
        "*1\r\n$3\r\n1-2"
    );
    let rows = pending_rows4(&shared, s, b"g", 10);
    let row = rows.iter().find(|r| r.0 == "1-2").expect("minted row");
    assert_eq!((row.1.as_str(), row.3), ("c2", 5), "{rows:?}");
}

#[test]
fn syntax_errors_on_bad_values() {
    let (shared, s) = seeded("45026");
    assert_eq!(
        claim(&shared, s, &[b"g", b"c2", b"0", b"1-1", b"IDLE", b"abc"]),
        NOT_INT
    );
    assert_eq!(
        claim(&shared, s, &[b"g", b"c2", b"0", b"1-1", b"TIME", b"-5"]),
        NOT_INT
    );
    assert_eq!(
        claim(&shared, s, &[b"g", b"c2", b"0", b"1-1", b"RETRYCOUNT"]),
        NOT_INT
    );
    assert_eq!(
        claim(&shared, s, &[b"g", b"c2", b"0", b"1-1", b"IDLE"]),
        NOT_INT
    );
    assert_eq!(
        claim(&shared, s, &[b"g", b"c2", b"0", b"1-1", b"FROB"]),
        "-ERR Invalid stream ID specified"
    );
    // A bad hint must not have mutated anything above.
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!((rows[0].1.as_str(), rows[0].3), ("c1", 1), "{rows:?}");
    // XAUTOCLAIM keeps its no-hint shape (hints are XCLAIM-only).
    let t = text(&call(
        &shared,
        "xautoclaim",
        &[s, b"g", b"c2", b"600000", b"-"],
    ));
    assert!(t.contains("ERR ") || t.starts_with("*"), "{t}");
}
