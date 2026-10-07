//! `XINFO STREAM <key> FULL` e2e + XADD NOMKSTREAM/trim (P3 backfill
//! items #8/#9): FULL vs non-FULL shape, COUNT capping of the entries /
//! PEL lists, group/consumer nesting, NOMKSTREAM's nil-without-create,
//! and the XADD-borne trim (incl. a MINID LIMIT case pinning the shared
//! XTRIM parameter semantics). In-process, real registry + real store.

mod common;

use common::lite::{add, call, mk_group, shared_at, text};
use rdb::state::Shared;

fn xlen(shared: &Shared, stream: &[u8]) -> i64 {
    text(&call(shared, "xlen", &[stream]))
        .trim_start_matches(':')
        .trim_end()
        .parse()
        .expect("xlen integer")
}

/// First entry id of the stream (`-`/`+` full range); "" when empty.
fn first_id(shared: &Shared, stream: &[u8]) -> String {
    text(&call(shared, "xrange", &[stream, b"-", b"+"]))
        .split("\r\n")
        .nth(3)
        .unwrap_or_default()
        .to_string()
}

/// Seed `1-1 ..= n-1`, deliver them all to `c1` of group `g1`.
fn seeded(tag: &str, n: u64) -> (Shared, &'static [u8]) {
    let (shared, _dir) = shared_at(tag);
    let s: &'static [u8] = b"xi/t0";
    for i in 1..=n {
        add(&shared, s, &format!("{i}-1"));
    }
    mk_group(&shared, s, b"g1");
    let t = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g1", b"c1", b"streams", s, b">"],
    ));
    assert!(t.contains(&format!("{}-1", n)), "{t}");
    (shared, s)
}

#[test]
fn full_shape_and_non_full_unchanged() {
    let (shared, s) = seeded("45120", 12);
    // Non-FULL: the historical four pairs, byte-for-byte.
    assert_eq!(
        text(&call(&shared, "xinfo", &[b"stream", s])),
        "*8\r\n$6\r\nlength\r\n:12\r\n$17\r\nlast-generated-id\r\n$4\r\n12-1\r\n\
         $6\r\ngroups\r\n:1\r\n$7\r\nidle-ms\r\n:0\r\n"
    );
    let full = text(&call(&shared, "xinfo", &[b"stream", s, b"FULL"]));
    let (head, tail) = full
        .split_once("$6\r\ngroups\r\n")
        .expect("groups section present");
    // Stream level: length, last-generated-id, entries (10 = default
    // COUNT cap, newest tail, oldest-first emission).
    assert!(head.contains("$6\r\nlength\r\n:12\r\n"), "{head}");
    assert!(
        head.contains("$17\r\nlast-generated-id\r\n$4\r\n12-1\r\n"),
        "{head}"
    );
    assert_eq!(head.matches("$1\r\nf\r\n$1\r\nv").count(), 10, "{head}");
    assert!(
        head.contains("entries\r\n*10\r\n*2\r\n$3\r\n3-1\r\n"),
        "{head}"
    );
    // Group level: name / last-delivered-id / pending list / consumers.
    assert!(tail.contains("$4\r\nname\r\n$2\r\ng1\r\n"), "{tail}");
    assert!(tail.contains("$17\r\nlast-delivered-id\r\n"), "{tail}");
    assert!(tail.contains("$7\r\npending\r\n*10\r\n"), "{tail}");
    // Pending rows: [id, consumer, time-since-delivered, deliveries].
    assert!(tail.contains("*4\r\n$3\r\n1-1\r\n$2\r\nc1\r\n:"), "{tail}");
    // Consumer level: name / seen-time / pending count / pel rows
    // [id, time-since-delivered, deliveries].
    assert!(tail.contains("$9\r\nconsumers\r\n*1\r\n*6\r\n"), "{tail}");
    assert!(
        tail.contains("$4\r\nname\r\n$2\r\nc1\r\n$9\r\nseen-time\r\n:"),
        "{tail}"
    );
    assert!(tail.contains("$7\r\npending\r\n:12\r\n"), "{tail}");
    assert!(
        tail.contains("$3\r\npel\r\n*10\r\n*3\r\n$3\r\n1-1\r\n:"),
        "{tail}"
    );
}

#[test]
fn count_caps_entries_and_pel_lists_only() {
    let (shared, s) = seeded("45121", 12);
    let full = text(&call(
        &shared,
        "xinfo",
        &[b"stream", s, b"FULL", b"COUNT", b"2"],
    ));
    let (head, tail) = full.split_once("$6\r\ngroups\r\n").expect("groups present");
    assert_eq!(head.matches("$1\r\nf\r\n$1\r\nv").count(), 2, "{head}");
    assert!(
        head.contains("entries\r\n*2\r\n*2\r\n$4\r\n11-1\r\n"),
        "{head}"
    );
    // Lists cap; exact counters do not (pending stays :12).
    assert!(tail.contains("$7\r\npending\r\n*2\r\n"), "{tail}");
    assert!(tail.contains("$7\r\npending\r\n:12\r\n"), "{tail}");
    assert!(tail.contains("$3\r\npel\r\n*2\r\n"), "{tail}");
}

#[test]
fn full_syntax_matrix() {
    let (shared, s) = seeded("45122", 2);
    assert_eq!(
        text(&call(
            &shared,
            "xinfo",
            &[b"stream", s, b"FULL", b"COUNT", b"x"]
        )),
        "-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        text(&call(&shared, "xinfo", &[b"stream", s, b"FULL", b"EXTRA"])),
        "-ERR Unknown subcommand or wrong number of arguments for 'xinfo' command\r\n"
    );
    assert_eq!(
        text(&call(
            &shared,
            "xinfo",
            &[b"stream", s, b"FULL", b"COUNT", b"0"]
        )),
        "-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        text(&call(&shared, "xinfo", &[b"stream", b"xi/none", b"FULL"])),
        "-ERR no such key\r\n"
    );
}

#[test]
fn nomkstream_nil_and_key_absent() {
    let (shared, _dir) = shared_at("45123");
    // Missing stream: nil bulk, and NO key materializes.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[b"nm/q0", b"NOMKSTREAM", b"*", b"f", b"v"]
        )),
        "$-1\r\n"
    );
    assert_eq!(xlen(&shared, b"nm/q0"), 0);
    assert_eq!(
        text(&call(&shared, "xinfo", &[b"stream", b"nm/q0"])),
        "-ERR no such key\r\n"
    );
    // Options before the id make the id mandatory (Redis grammar).
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[b"nm/q0", b"NOMKSTREAM", b"f", b"v"]
        )),
        "-ERR wrong number of arguments for 'xadd' command\r\n"
    );
    // Existing stream: NOMKSTREAM is a no-op, the append lands.
    add(&shared, b"nm/q0", "1-1");
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[b"nm/q0", b"NOMKSTREAM", b"2-1", b"f", b"v"]
        )),
        "$3\r\n2-1\r\n"
    );
    assert_eq!(xlen(&shared, b"nm/q0"), 2);
}

#[test]
fn xadd_trim_pins_xtrim_limit_semantics() {
    let (shared, _dir) = shared_at("45124");
    let s: &'static [u8] = b"tr/q0";
    for n in 1..=5u64 {
        add(&shared, s, &format!("{n}-1"));
    }
    // #9b: XADD's trim options share XTRIM's parser: MINID + LIMIT
    // caps THIS call's deletions (LIMIT stops before a victim is
    // taken), the remainder stays for a later call.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[s, b"MINID", b"=", b"3-1", b"LIMIT", b"1", b"6-1", b"f", b"v"]
        )),
        "$3\r\n6-1\r\n"
    );
    assert_eq!(xlen(&shared, s), 5);
    assert_eq!(first_id(&shared, s), "2-1");
    // Follow-up without LIMIT finishes the MINID cut.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[s, b"MINID", b"3-1", b"7-1", b"f", b"v"]
        )),
        "$3\r\n7-1\r\n"
    );
    assert_eq!(xlen(&shared, s), 5);
    assert_eq!(first_id(&shared, s), "3-1");
    // MAXLEN keeps the newest n (new entry included in the count).
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[s, b"MAXLEN", b"~", b"3", b"8-1", b"f", b"v"]
        )),
        "$3\r\n8-1\r\n"
    );
    assert_eq!(xlen(&shared, s), 3);
    assert_eq!(first_id(&shared, s), "6-1");
    // Trim + NOMKSTREAM on a missing stream: still nil, still no key.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[b"tr/q9", b"NOMKSTREAM", b"MAXLEN", b"2", b"*", b"f", b"v"]
        )),
        "$-1\r\n"
    );
    assert_eq!(xlen(&shared, b"tr/q9"), 0);
    // Syntax errors keep the XTRIM family's reply texts.
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[s, b"MAXLEN", b"x", b"9-1", b"f", b"v"]
        )),
        "-ERR value is not an integer or out of range\r\n"
    );
    assert_eq!(
        text(&call(
            &shared,
            "xadd",
            &[s, b"MINID", b"zz", b"9-1", b"f", b"v"]
        )),
        "-ERR Invalid stream ID specified as stream command argument\r\n"
    );
    // A non-option leading token is a FIELD of the plain dialect (the
    // id elision still applies); only a malformed tail errors.
    assert_eq!(
        text(&call(&shared, "xadd", &[s, b"FROB"])),
        "-ERR wrong number of arguments for 'xadd' command\r\n"
    );
}
