//! XTRIM MINID e2e (in-process, real registry + real store): exact and
//! approximate forms, LIMIT capping, time-window retention via `<ms>-0`
//! ids, edge/syntax matrices, orthogonality with MAXLEN, and the kafka
//! committed-offset guard (kind 0x20 rows reject XTRIM/XDEL until the
//! ledger is cleared).

mod common;

use common::lite::{call, shared_at, text};
use rdb::state::Shared;

/// XADD with an explicit id, asserting the echoed id (byte-deterministic
/// replies keep every later assertion exact).
fn add(shared: &Shared, stream: &[u8], id: &str) {
    assert_eq!(
        call(shared, "xadd", &[stream, id.as_bytes(), b"f", b"v"]),
        format!("${}\r\n{id}\r\n", id.len()).into_bytes(),
        "xadd {id}"
    );
}

fn xtrim(shared: &Shared, stream: &[u8], args: &[&[u8]]) -> String {
    let mut argv = vec![stream];
    argv.extend_from_slice(args);
    text(&call(shared, "xtrim", &argv)).trim_end().to_string()
}

fn xlen(shared: &Shared, stream: &[u8]) -> i64 {
    text(&call(shared, "xlen", &[stream]))
        .trim_start_matches(':')
        .trim_end()
        .parse()
        .expect("xlen integer")
}

/// First entry id of the stream (`-`/`+` full range). The reply frame
/// is `*N` / per-entry `*2` / `$len` / `<id>` / ..., so the id is the
/// 4th `\r\n`-separated token; "" when the stream is empty.
fn first_id(shared: &Shared, stream: &[u8]) -> String {
    let t = text(&call(shared, "xrange", &[stream, b"-", b"+"]));
    t.split("\r\n").nth(3).unwrap_or_default().to_string()
}

/// Seed 5 entries `1-1 ..= 5-1`.
fn seed_five(shared: &Shared, stream: &[u8]) {
    for n in 1..=5u64 {
        add(shared, stream, &format!("{n}-1"));
    }
}

#[test]
fn minid_exact_and_approx_forms() {
    let (shared, _dir) = shared_at("44320");

    // Exact `=`: everything strictly below the id goes, the boundary
    // entry itself survives.
    let s1 = b"s1/q0".as_slice();
    seed_five(&shared, s1);
    assert_eq!(xtrim(&shared, s1, &[b"MINID", b"=", b"3-1"]), ":2");
    assert_eq!(xlen(&shared, s1), 3);
    assert_eq!(first_id(&shared, s1), "3-1");

    // Approximate `~` is ACCEPTED for wire compatibility but implemented
    // exactly like `=`: victims are computed precisely (never
    // under-deletes), so the approximation is a no-op refinement. (Each
    // case seeds its own stream: XADD refuses ids below the last id.)
    let s2 = b"s2/q0".as_slice();
    seed_five(&shared, s2);
    assert_eq!(xtrim(&shared, s2, &[b"MINID", b"~", b"3-1"]), ":2");
    assert_eq!(first_id(&shared, s2), "3-1");

    // LIMIT caps one round; the remainder is a later call's work
    // (ours takes LIMIT after both `~` and `=`, same semantics).
    let s3 = b"s3/q0".as_slice();
    seed_five(&shared, s3);
    assert_eq!(
        xtrim(&shared, s3, &[b"MINID", b"~", b"3-1", b"LIMIT", b"1"]),
        ":1"
    );
    assert_eq!(xlen(&shared, s3), 4);
    assert_eq!(first_id(&shared, s3), "2-1");
    assert_eq!(
        xtrim(&shared, s3, &[b"MINID", b"=", b"3-1"]),
        ":1",
        "tail cleanup"
    );
    assert_eq!(xlen(&shared, s3), 3);

    // A minid equal to (or below) the first id removes nothing.
    let s4 = b"s4/q0".as_slice();
    seed_five(&shared, s4);
    assert_eq!(xtrim(&shared, s4, &[b"MINID", b"=", b"1-1"]), ":0");
    assert_eq!(xtrim(&shared, s4, &[b"MINID", b"~", b"0-1"]), ":0");
    assert_eq!(xlen(&shared, s4), 5);

    // LIMIT 0 is a ZERO budget, not "one": it must delete nothing and
    // reply 0 (the old walk pushed the first victim before checking
    // the cap -- an off-by-one that ate one entry).
    let s5 = b"s5/q0".as_slice();
    seed_five(&shared, s5);
    assert_eq!(
        xtrim(&shared, s5, &[b"MINID", b"~", b"3-1", b"LIMIT", b"0"]),
        ":0"
    );
    assert_eq!(xlen(&shared, s5), 5, "LIMIT 0 deletes nothing");
    assert_eq!(first_id(&shared, s5), "1-1");
    assert_eq!(
        xtrim(&shared, s5, &[b"MINID", b"=", b"3-1", b"LIMIT", b"0"]),
        ":0"
    );
    assert_eq!(xlen(&shared, s5), 5);
}

#[test]
fn minid_time_window_retention() {
    let (shared, _dir) = shared_at("44321");
    let s = b"win/q0".as_slice();
    // Ids as `<ms timestamp>-0`: MINID over a `now - window` cutoff is
    // the whole retention story -- one periodic command, no per-message
    // index. Here: entries at t=1000/2000/3000/3500, keep >= 2000.
    for ms in [1000u64, 2000, 3000, 3500] {
        add(&shared, s, &format!("{ms}-0"));
    }
    assert_eq!(xtrim(&shared, s, &[b"MINID", b"~", b"2000-0"]), ":1");
    assert_eq!(xlen(&shared, s), 3);
    assert_eq!(first_id(&shared, s), "2000-0");
    // Sliding the window forward drops exactly the now-stale prefix
    // (both 2000-0 and 3000-0 sit below the new cutoff 3001-0).
    assert_eq!(xtrim(&shared, s, &[b"MINID", b"~", b"3001-0"]), ":2");
    assert_eq!(first_id(&shared, s), "3500-0");
    assert_eq!(xlen(&shared, s), 1);
}

#[test]
fn minid_edge_and_syntax_errors() {
    let (shared, _dir) = shared_at("44322");
    let s = b"edge/q0".as_slice();
    // Missing stream and an empty-result window both reply 0.
    assert_eq!(xtrim(&shared, b"nope/q0", &[b"MINID", b"=", b"5-5"]), ":0");
    add(&shared, s, "7-1");
    assert_eq!(xtrim(&shared, s, &[b"MINID", b"=", b"7-1"]), ":0");
    // Syntax matrices: missing id, malformed id, bad LIMIT, unknown
    // strategy, and MAXLEN arity behavior unchanged.
    assert!(xtrim(&shared, s, &[b"MINID"]).starts_with("-ERR"));
    assert!(xtrim(&shared, s, &[b"MINID", b"=", b"bogus"]).starts_with("-ERR"));
    assert!(xtrim(&shared, s, &[b"MINID", b"=", b"7-1", b"LIMIT", b"x"]).starts_with("-ERR"));
    assert!(xtrim(&shared, s, &[b"BOGUS", b"5"]).starts_with("-ERR"));
    assert!(xtrim(&shared, s, &[b"MAXLEN", b"=", b"1", b"LIMIT", b"1"]).starts_with("-ERR"));
    // Nothing above touched the entry.
    assert_eq!(xlen(&shared, s), 1);
}

#[test]
fn minid_and_maxlen_orthogonal() {
    let (shared, _dir) = shared_at("44323");
    let s = b"mix/q0".as_slice();
    for n in 1..=6u64 {
        add(&shared, s, &format!("{n}-1"));
    }
    // The two strategies alternate freely: each call applies only its
    // own window computation to the CURRENT entry set.
    assert_eq!(xtrim(&shared, s, &[b"MAXLEN", b"=", b"4"]), ":2");
    assert_eq!(xlen(&shared, s), 4);
    assert_eq!(first_id(&shared, s), "3-1");
    assert_eq!(xtrim(&shared, s, &[b"MINID", b"=", b"4-1"]), ":1");
    assert_eq!(xlen(&shared, s), 3);
    assert_eq!(first_id(&shared, s), "4-1");
    assert_eq!(xtrim(&shared, s, &[b"MAXLEN", b"~", b"10"]), ":0");
    assert_eq!(xtrim(&shared, s, &[b"MINID", b"~", b"4-1"]), ":0");
    assert_eq!(xlen(&shared, s), 3);
    assert_eq!(
        xtrim(&shared, s, &[b"MINID", b"=", b"6-1", b"LIMIT", b"5"]),
        ":2"
    );
    assert_eq!(xlen(&shared, s), 1);
    assert_eq!(first_id(&shared, s), "6-1");
}

#[test]
fn kafka_ledger_guard_blocks_xtrim_and_xdel() {
    let (shared, _dir) = shared_at("44324");
    let s = b"t/q0".as_slice();
    for n in 1..=3u64 {
        add(&shared, s, &format!("{n}-1"));
    }
    // Lay one committed-offset ledger row (kind 0x20) the way the kafka
    // front does: under the PARENT-derived slot prefix, keyed by the
    // full stream name + "/group".
    let prefix = rdb::hash::slot_with_prefix(b"t").1;
    let mut batch = rocksdb::WriteBatch::default();
    rdb::kafka::ledger::put_rows(
        &mut batch,
        &[rdb::kafka::ledger::LedgerRow {
            stream: s.to_vec(),
            group: b"g1".to_vec(),
            prefix: prefix.clone(),
            committed_ordinal: 2,
            generation: 1,
            leader: "m-1".into(),
        }],
    );
    rdb::store::ops::batch_write(&shared.store, batch).expect("ledger row");

    // Both XTRIM strategies and XDEL are refused with the dedicated
    // text; the stream is left untouched.
    for args in [
        vec![b"MAXLEN".as_slice(), b"=", b"1"],
        vec![b"MINID".as_slice(), b"=", b"0-1"],
    ] {
        let reply = xtrim(&shared, s, &args);
        assert!(
            reply.contains("consumer-group offsets"),
            "guard text: {reply}"
        );
        assert!(reply.starts_with("-ERR"), "error frame: {reply}");
    }
    let xdel = text(&call(&shared, "xdel", &[s, b"1-1"]));
    assert!(
        xdel.contains("consumer-group offsets"),
        "xdel guard: {xdel}"
    );
    assert_eq!(xlen(&shared, s), 3, "nothing was trimmed/deleted");

    // Clearing the ledger row (what a group delete / stream purge does
    // through the family-fold path) releases the guard.
    let mut batch = rocksdb::WriteBatch::default();
    batch.delete(rdb::kafka::ledger::ledger_key(&prefix, s, b"g1"));
    rdb::store::ops::batch_write(&shared.store, batch).expect("ledger clear");
    assert_eq!(xtrim(&shared, s, &[b"MAXLEN", b"=", b"1"]), ":2");
    assert_eq!(text(&call(&shared, "xdel", &[s, b"3-1"])).trim_end(), ":1");
    assert_eq!(xlen(&shared, s), 0);
}

/// Lay one committed-offset ledger row (kind 0x20) under the stream's
/// PARENT-derived slot prefix, exactly like the kafka front does.
fn ledger_row(shared: &Shared, stream: &[u8], group: &[u8], ordinal: u64) {
    let parent = stream.split(|&b| b == b'/').next().unwrap_or_default();
    let prefix = rdb::hash::slot_with_prefix(parent).1;
    let mut batch = rocksdb::WriteBatch::default();
    rdb::kafka::ledger::put_rows(
        &mut batch,
        &[rdb::kafka::ledger::LedgerRow {
            stream: stream.to_vec(),
            group: group.to_vec(),
            prefix,
            committed_ordinal: ordinal,
            generation: 1,
            leader: "m-1".into(),
        }],
    );
    rdb::store::ops::batch_write(&shared.store, batch).expect("ledger row");
}

/// XGROUP DESTROY is the ledger guard's NAMED exit: the destroy folds
/// the group's kind-0x20 rows (bounded to stream+group) so XTRIM MINID
/// goes through afterwards. Covers a lite group carrying a ledger row,
/// a kafka-ONLY group (ledger rows, no lite group record -- still
/// destroyable, still replies 1), and the g1-vs-g10 name-prefix hazard
/// (destroying g1 must leave g10's rows pinning the guard).
#[test]
fn xgroup_destroy_folds_ledger_rows_and_releases_the_guard() {
    let (shared, _dir) = shared_at("44325");

    // (1) A lite group whose name also carries a kafka ledger row: the
    // destroy tears down group + PEL + ledger in one batch.
    let s1 = b"gz/q0".as_slice();
    seed_five(&shared, s1);
    assert_eq!(
        text(&call(&shared, "xgroup", &[b"create", s1, b"gl", b"0-0"])),
        "+OK\r\n"
    );
    assert!(text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"gl", b"c1", b"streams", s1, b">"]
    ))
    .contains("1-1"));
    ledger_row(&shared, s1, b"gl", 2);
    let reply = xtrim(&shared, s1, &[b"MINID", b"=", b"0-1"]);
    assert!(
        reply.contains("consumer-group offsets"),
        "guard armed: {reply}"
    );
    assert!(
        reply.contains("XGROUP DESTROY"),
        "guard names the exit: {reply}"
    );
    assert_eq!(
        text(&call(&shared, "xgroup", &[b"destroy", s1, b"gl"])).trim_end(),
        ":1"
    );
    // The group is gone (XPENDING answers NOGROUP) and so is its PEL.
    assert!(text(&call(&shared, "xpending", &[s1, b"gl"])).contains("NOGROUP"));
    assert_eq!(
        xtrim(&shared, s1, &[b"MINID", b"=", b"3-1"]),
        ":2",
        "guard released"
    );

    // (2) A kafka-ONLY group: ledger rows but no lite group record.
    let s2 = b"ko/q0".as_slice();
    seed_five(&shared, s2);
    ledger_row(&shared, s2, b"kg", 4);
    assert!(xtrim(&shared, s2, &[b"MINID", b"=", b"0-1"]).starts_with("-ERR"));
    assert_eq!(
        text(&call(&shared, "xgroup", &[b"destroy", s2, b"kg"])).trim_end(),
        ":1",
        "kafka-only group existed (its rows did)"
    );
    assert_eq!(xtrim(&shared, s2, &[b"MINID", b"=", b"3-1"]), ":2");

    // (3) The destroy stays bounded to stream+group: g1's rows fold,
    // g10's survive (no prefix swallow), the guard stays armed until
    // g10 itself is destroyed. A group with nothing anywhere is :0.
    let s3 = b"pf/q0".as_slice();
    seed_five(&shared, s3);
    ledger_row(&shared, s3, b"g1", 1);
    ledger_row(&shared, s3, b"g10", 1);
    assert_eq!(
        text(&call(&shared, "xgroup", &[b"destroy", s3, b"g1"])).trim_end(),
        ":1"
    );
    assert!(
        xtrim(&shared, s3, &[b"MINID", b"=", b"0-1"]).starts_with("-ERR"),
        "g10's rows still pin the stream"
    );
    assert_eq!(
        text(&call(&shared, "xgroup", &[b"destroy", s3, b"nope"])).trim_end(),
        ":0",
        "nothing existed under that name"
    );
    assert_eq!(
        text(&call(&shared, "xgroup", &[b"destroy", s3, b"g10"])).trim_end(),
        ":1"
    );
    assert_eq!(xtrim(&shared, s3, &[b"MINID", b"=", b"3-1"]), ":2");
}
