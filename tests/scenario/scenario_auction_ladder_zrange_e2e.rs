//! Multi-store scenario #132 -- an auction ladder: twelve bids on one
//! item live as a zset (member = bid id, score = amount), as rows of a
//! SQL table and as entries of the acceptance stream. The unified ZRANGE
//! (BYSCORE / BYLEX / REV / LIMIT / WITHSCORES) must answer byte-for-byte
//! what the legacy twins answer, both agree with a Rust-side ranking,
//! `ORDER BY amount DESC, bid_id ASC LIMIT 2 OFFSET 1` over the SQL rows
//! returns the same window as `ZRANGE key +inf -inf BYSCORE REV LIMIT 1 2`,
//! the stream's entries follow the acceptance order and its last entry
//! is the top bid (`ZREVRANGE key 0 0`), and invalid option combos are
//! refused with Redis's error texts. SURFACES: RESP (zset + stream) +
//! MySQL. One REAL node, MySQL.
//!
//! Pinned (deviations, asserted as they are):
//! * under BYLEX the REV switch does NOT swap the bounds (BYSCORE does):
//!   `ZRANGE key [max [min BYLEX REV` is an EMPTY window; the ascending
//!   bounds give the ZREVRANGEBYLEX reply, src/command/zset_range.rs:212-220
//!   (legacy twin swaps: :391-395). Redis 6.2 swaps in both cases.
//! * on a MIXED-score zset the BYLEX windows answer in score order -- the
//!   lex filter keeps `collect_scored`'s (score, member) order,
//!   src/command/zset_range.rs:150-170 (Redis leaves that order
//!   unspecified). Unified and legacy agree byte-for-byte and the member
//!   SET is checked on the Rust side; the lex ORDER itself is asserted on
//!   an equal-score tie ladder, where the two contracts coincide.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, run, s};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, stream_ids, FrontSpec};
use std::time::Duration;

const TAG: &str = "zr132";
const ITEM: &str = "guitar";
const LADDER: &str = "auction/guitar/ladder";
const TIES: &str = "auction/guitar/ties";
const STREAM: &str = "auction/guitar/bids";

/// The twelve bids in ACCEPTANCE order: (bid id, amount). Amounts are
/// distinct and not monotonic in id order; the last accepted bid is the
/// top bid (the stream's tail).
const BIDS: [(&str, i64); 12] = [
    ("bid-000", 420),
    ("bid-001", 150),
    ("bid-002", 610),
    ("bid-003", 275),
    ("bid-004", 330),
    ("bid-005", 540),
    ("bid-006", 210),
    ("bid-007", 455),
    ("bid-008", 760),
    ("bid-009", 880),
    ("bid-010", 305),
    ("bid-011", 980),
];

/// The ladder ascending by amount (ties by id -- none here).
fn rank() -> Vec<(&'static str, i64)> {
    let mut v = BIDS.to_vec();
    v.sort_by_key(|&(id, amount)| (amount, id));
    v
}

/// Descending window: `skip` entries skipped from the top, `count` taken.
fn top_window(skip: usize, count: usize) -> Vec<&'static str> {
    rank()
        .iter()
        .rev()
        .skip(skip)
        .take(count)
        .map(|(id, _)| *id)
        .collect()
}

fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Single-line reply text (`+OK`, `-ERR ...`, `:N`).
async fn cmd(addr: &str, args: &[&str]) -> String {
    common::lite::text(&r(addr, &argv(args)).await)
}

/// Full-drain reply text (arrays): quiet-gap reader, 25ms.
async fn drain(addr: &str, args: &[&str]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(addr, common::TOKEN, &argv(args), 25, 10_000).await)
}

/// Wire-order cells of a reply: `:n` -> "n", `$len` -> the payload that
/// follows (null -> ""), array heads skipped.
fn cells(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        if let Some(n) = t.strip_prefix(':') {
            out.push(n.to_string());
        } else if let Some(len) = t.strip_prefix('$') {
            if let Ok(n) = len.parse::<usize>() {
                out.push(if n == 0 { String::new() } else { toks.get(i + 1).unwrap_or(&"").to_string() });
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// Members of a plain (no WITHSCORES) array reply.
fn members(full: &str) -> Vec<String> {
    cells(full)
}

/// `(member, score)` pairs of a WITHSCORES array reply.
fn pairs(full: &str) -> Vec<(String, String)> {
    let c = cells(full);
    assert_eq!(c.len() % 2, 0, "WITHSCORES shape: {full}");
    c.chunks(2).map(|p| (p[0].clone(), p[1].clone())).collect()
}

fn cell(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("expected text cell, got {other:?}"),
    }
}

#[tokio::test]
async fn unified_zrange_matches_legacy_and_sql_order() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { mysql: true, ..Default::default() };
    let (front, a) = front_world(TAG, &spec).await;
    let mut c = connect_root(&front.node).await;
    let ladder = rank();

    // a) Seed the ladder (one ZADD of 12 bids) and the tie ladder.
    let mut zadd = vec!["ZADD".to_string(), LADDER.to_string()];
    for (id, amount) in &BIDS {
        zadd.push(amount.to_string());
        zadd.push((*id).to_string());
    }
    let zadd_ref: Vec<&str> = zadd.iter().map(String::as_str).collect();
    assert_eq!(cmd(&a, &zadd_ref).await, ":12", "twelve bids added");
    assert_eq!(cmd(&a, &["ZADD", TIES, "500", "bid-c", "500", "bid-a", "500", "bid-d", "500", "bid-b"]).await, ":4");
    assert_eq!(cmd(&a, &["ZCARD", LADDER]).await, ":12");

    // b) The matrix: every unified form against its legacy twin.
    let t = scenario_common::perf::now();
    let matrix: [(&[&str], &[&str]); 10] = [
        (&["ZRANGE", LADDER, "-inf", "+inf", "BYSCORE"], &["ZRANGEBYSCORE", LADDER, "-inf", "+inf"]),
        (&["ZRANGE", LADDER, "+inf", "-inf", "BYSCORE", "REV", "LIMIT", "1", "2"], &["ZREVRANGEBYSCORE", LADDER, "+inf", "-inf", "LIMIT", "1", "2"]),
        (&["ZRANGE", LADDER, "300", "(540", "BYSCORE"], &["ZRANGEBYSCORE", LADDER, "300", "(540"]),
        (&["ZRANGE", LADDER, "(300", "610", "BYSCORE", "WITHSCORES"], &["ZRANGEBYSCORE", LADDER, "(300", "610", "WITHSCORES"]),
        (&["ZRANGE", LADDER, "610", "(300", "BYSCORE", "REV", "WITHSCORES", "LIMIT", "0", "2"], &["ZREVRANGEBYSCORE", LADDER, "610", "(300", "WITHSCORES", "LIMIT", "0", "2"]),
        (&["ZRANGE", LADDER, "[bid-003", "[bid-007", "BYLEX"], &["ZRANGEBYLEX", LADDER, "[bid-003", "[bid-007"]),
        (&["ZRANGE", LADDER, "0", "2", "REV"], &["ZREVRANGE", LADDER, "0", "2"]),
        (&["ZRANGE", LADDER, "0", "2", "REV", "WITHSCORES"], &["ZREVRANGE", LADDER, "0", "2", "WITHSCORES"]),
        (&["ZRANGE", LADDER, "+inf", "-inf", "BYSCORE", "REV", "WITHSCORES", "LIMIT", "0", "1"], &["ZREVRANGEBYSCORE", LADDER, "+inf", "-inf", "WITHSCORES", "LIMIT", "0", "1"]),
        (&["ZRANGE", TIES, "[bid-b", "[bid-d", "BYLEX"], &["ZRANGEBYLEX", TIES, "[bid-b", "[bid-d"]),
    ];
    let mut unified = Vec::new();
    let mut legacy = Vec::new();
    for (u, l) in &matrix {
        unified.push(drain(&a, u).await);
        legacy.push(drain(&a, l).await);
    }
    let rank_all = drain(&a, &["ZRANGE", LADDER, "0", "-1"]).await;
    let rank_all_rev = drain(&a, &["ZREVRANGE", LADDER, "0", "-1"]).await;
    let tail = drain(&a, &["ZRANGE", LADDER, "-3", "-1"]).await;
    let asc_window = drain(&a, &["ZRANGE", LADDER, "-inf", "+inf", "BYSCORE", "LIMIT", "4", "3"]).await;
    let ties_all = drain(&a, &["ZRANGEBYLEX", TIES, "-", "+"]).await;
    // The BYSCORE/BYLEX/REV switches FOLLOW min max (Redis 6.2 form), and
    // argv[0] (the command name) is stripped before the handler, so arity
    // counts key+min+max: `key 0` is the arity error, `key BYSCORE 1` has
    // three args and BYSCORE is read as the (non-integer) start index,
    // src/command/zset_range.rs:186-243.
    let errors: [(Vec<&str>, &str); 11] = [
        (vec!["ZRANGE", LADDER, "0", "1", "LIMIT", "0", "1"], "-ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX"),
        (vec!["ZRANGE", LADDER, "0", "1", "REV", "LIMIT", "0", "1"], "-ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX"),
        (vec!["ZRANGE", LADDER, "abc", "500", "BYSCORE"], "-ERR min or max not valid float"),
        (vec!["ZRANGE", LADDER, "BYSCORE", "1"], "-ERR value is not an integer or out of range"),
        (vec!["ZRANGE", LADDER, "0"], "-ERR wrong number of arguments for 'zrange' command"),
        (vec!["ZRANGE", LADDER, "1", "5", "BYLEX", "WITHSCORES"], "-ERR syntax error, WITHSCORES not supported in combination with BYLEX"),
        (vec!["ZRANGE", LADDER, "bad", "5", "BYLEX"], "-ERR min or max not valid string range item"),
        (vec!["ZRANGE", LADDER, "1", "2", "BYSCORE", "BYLEX"], "-ERR syntax error"),
        (vec!["ZRANGE", LADDER, "0", "1", "LIMIT", "1"], "-ERR syntax error"),
        (vec!["ZRANGE", LADDER, "0", "1", "LIMIT", "a", "b"], "-ERR value is not an integer or out of range"),
        (vec!["ZREVRANGE", LADDER, "0", "1", "LIMIT", "0", "1"], "-ERR syntax error"),
    ];
    let mut got_errors = Vec::new();
    for (args, _) in &errors {
        got_errors.push(cmd(&a, args).await);
    }
    let legacy_errors = [
        cmd(&a, &["ZRANGEBYSCORE", LADDER, "0", "1", "REV"]).await,
        cmd(&a, &["ZRANGEBYLEX", LADDER, "-", "+", "WITHSCORES"]).await,
        cmd(&a, &["ZREVRANGEBYLEX", LADDER, "+", "-", "WITHSCORES"]).await,
        cmd(&a, &["ZREVRANGEBYSCORE", LADDER, "abc", "-inf"]).await,
    ];
    let lex_count = cmd(&a, &["ZLEXCOUNT", LADDER, "[bid-003", "[bid-007"]).await;
    scenario_common::perf::within("zr132/zrange-matrix", t, Duration::from_secs(3));

    // c) Unified == legacy, byte for byte, and == the Rust-side ranking.
    for ((u, l), (x, y)) in matrix.iter().zip(unified.iter().zip(&legacy)) {
        assert_eq!(x, y, "unified {u:?} vs legacy {l:?}");
    }
    let ascending: Vec<String> = ladder.iter().map(|(id, _)| id.to_string()).collect();
    assert_eq!(members(&unified[0]), ascending, "BYSCORE -inf +inf is the ascending ladder");
    assert_eq!(members(&rank_all), ascending, "rank 0 -1 == BYSCORE -inf +inf");
    assert_eq!(members(&rank_all_rev), ascending.iter().rev().cloned().collect::<Vec<_>>());
    assert_eq!(members(&unified[1]), vec!["bid-009", "bid-008"], "REV window: ranks 1..2");
    assert_eq!(members(&unified[1]), top_window(1, 2));
    assert_eq!(members(&unified[2]), vec!["bid-010", "bid-004", "bid-000", "bid-007"], "300 <= s < 540");
    assert_eq!(members(&unified[2]), ladder.iter().filter(|&&(_, s)| (300..540).contains(&s)).map(|(id, _)| id.to_string()).collect::<Vec<_>>());
    assert_eq!(pairs(&unified[3]), vec![("bid-010".into(), "305".into()), ("bid-004".into(), "330".into()), ("bid-000".into(), "420".into()), ("bid-007".into(), "455".into()), ("bid-005".into(), "540".into()), ("bid-002".into(), "610".into())], "300 < s <= 610");
    assert_eq!(pairs(&unified[4]), vec![("bid-002".into(), "610".into()), ("bid-005".into(), "540".into())], "REV reads (max, min)");
    let mut lex_set: Vec<String> = members(&unified[5]);
    lex_set.sort();
    assert_eq!(lex_set, vec!["bid-003", "bid-004", "bid-005", "bid-006", "bid-007"], "lex window = members in [bid-003, bid-007]");
    assert_eq!(lex_count, ":5", "ZLEXCOUNT agrees with the window");
    // DEVIATION (pinned, not in the byte-equal matrix): under BYLEX the REV
    // switch does NOT swap the bounds (BYSCORE does) -- `zrange` hands
    // (args[1], args[2]) to lex_window_reply unswapped, while the legacy
    // twin swaps them, src/command/zset_range.rs:212-220 vs :391-395. So
    // the Redis form `[max [min BYLEX REV` is an EMPTY window, and the
    // ASCENDING bounds reproduce the ZREVRANGEBYLEX reply (reverse of the
    // forward window, LIMIT counted in reply order).
    let lex_rev_redis_form = drain(&a, &["ZRANGE", LADDER, "[bid-007", "[bid-003", "BYLEX", "REV", "LIMIT", "0", "2"]).await;
    let lex_rev_asc_bounds = drain(&a, &["ZRANGE", LADDER, "[bid-003", "[bid-007", "BYLEX", "REV", "LIMIT", "0", "2"]).await;
    let lex_rev_legacy = drain(&a, &["ZREVRANGEBYLEX", LADDER, "[bid-007", "[bid-003", "LIMIT", "0", "2"]).await;
    assert_eq!(lex_rev_redis_form, "*0\r\n", "ZRANGE key [max [min BYLEX REV: bounds are not swapped -> empty window");
    assert_eq!(lex_rev_asc_bounds, lex_rev_legacy, "ascending bounds under BYLEX REV == ZREVRANGEBYLEX");
    assert_eq!(members(&lex_rev_asc_bounds), members(&unified[5]).iter().rev().take(2).cloned().collect::<Vec<_>>());
    assert_eq!(drain(&a, &["ZRANGE", TIES, "[bid-d", "[bid-b", "BYLEX", "REV", "LIMIT", "0", "2"]).await, "*0\r\n", "tie ladder: Redis form is empty too");
    assert_eq!(members(&drain(&a, &["ZRANGE", TIES, "[bid-b", "[bid-d", "BYLEX", "REV", "LIMIT", "0", "2"]).await), vec!["bid-d", "bid-c"], "tie ladder: ascending bounds, REV -> reverse lex order");
    assert_eq!(members(&unified[6]), vec!["bid-011", "bid-009", "bid-008"], "top three by rank");
    assert_eq!(members(&unified[6]), top_window(0, 3));
    assert_eq!(pairs(&unified[7]), vec![("bid-011".into(), "980".into()), ("bid-009".into(), "880".into()), ("bid-008".into(), "760".into())]);
    assert_eq!(pairs(&unified[8]), vec![("bid-011".into(), "980".into())]);
    assert_eq!(members(&tail), vec!["bid-008", "bid-009", "bid-011"], "negative ranks -3..-1 = the last three, ascending");
    assert_eq!(members(&asc_window), vec!["bid-004", "bid-000", "bid-007"], "ascending LIMIT 4 3 = ranks 4..6");
    assert_eq!(members(&unified[9]), vec!["bid-b", "bid-c", "bid-d"], "tie ladder: lex order");
    assert_eq!(members(&ties_all), vec!["bid-a", "bid-b", "bid-c", "bid-d"], "equal scores: lex == score order");
    // d) Error texts.
    for ((args, want), got) in errors.iter().zip(&got_errors) {
        assert_eq!(got, want, "{args:?}");
    }
    assert_eq!(legacy_errors, ["-ERR syntax error", "-ERR syntax error", "-ERR syntax error", "-ERR min or max not valid float"]);

    // e) The acceptance stream: one entry per bid, in acceptance order;
    //    the tail is the top bid.
    for (id, amount) in &BIDS {
        let id_s = amount.to_string();
        let reply = r(&a, &argv(&["XADD", STREAM, "*", "bidder", id, "amount", &id_s])).await;
        assert!(scenario_common::bulk_at(&reply, 0).contains('-'), "xadd {id}");
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", STREAM.as_bytes()]).await), 12);
    let xrange = drain(&a, &["XRANGE", STREAM, "-", "+"]).await;
    let ids = stream_ids(&xrange);
    assert_eq!(ids.len(), 12, "twelve entries: {xrange}");
    assert_eq!(ids, {
        let mut s = ids.clone();
        s.sort();
        s
    }, "stream ids ascend");
    let entries = cells(&xrange);
    assert_eq!(entries.len(), 60, "12 x (id, bidder, <id>, amount, <amount>)");
    for (i, (id, amount)) in BIDS.iter().enumerate() {
        let e = &entries[i * 5..i * 5 + 5];
        assert_eq!(e[0], ids[i]);
        assert_eq!((&e[1][..], &e[2][..], &e[3][..], &e[4][..]), ("bidder", *id, "amount", amount.to_string().as_str()));
    }
    let top = members(&drain(&a, &["ZREVRANGE", LADDER, "0", "0"]).await);
    assert_eq!(top, vec![BIDS[11].0], "stream tail == ZREVRANGE 0 0");
    assert_eq!(entries[59], "980");
    assert_eq!(cmd(&a, &["ZSCORE", LADDER, BIDS[11].0]).await, "$3\r\n980");

    // f) SQL: the same twelve bids as rows; the ORDER BY window equals
    //    the REV-BYSCORE-with-LIMIT window; bid_id follows acceptance.
    ddl(&mut c, "CREATE TABLE auction_bids (bid_id BIGINT PRIMARY KEY AUTO_INCREMENT, item VARCHAR(32), bidder VARCHAR(32), amount BIGINT)").await;
    let values: Vec<String> = BIDS.iter().map(|(id, amount)| format!("('{ITEM}', '{id}', {amount})")).collect();
    run(&mut c, &format!("INSERT INTO auction_bids (item, bidder, amount) VALUES {}", values.join(", "))).await;
    assert_eq!(common::mysql::ints(&mut c, "SELECT COUNT(*) FROM auction_bids WHERE item = 'guitar'").await, vec![12]);
    let stmt = c.prep("SELECT bidder FROM auction_bids WHERE item = ? ORDER BY amount DESC, bid_id ASC LIMIT 2 OFFSET 1").await.expect("prepare window");
    let rs: Vec<mysql_async::Row> = c.exec(&stmt, (ITEM,)).await.expect("exec window");
    let window: Vec<String> = rs.iter().map(|row| cell(&row.get::<MVal, _>(0).unwrap_or(MVal::NULL))).collect();
    assert_eq!(window, members(&unified[1]), "SQL window == ZRANGE +inf -inf BYSCORE REV LIMIT 1 2");
    let stmt2 = c.prep("SELECT bidder FROM auction_bids WHERE item = ? ORDER BY amount ASC, bid_id ASC LIMIT 3 OFFSET 4").await.expect("prepare asc window");
    let rs2: Vec<mysql_async::Row> = c.exec(&stmt2, (ITEM,)).await.expect("exec asc window");
    let asc: Vec<String> = rs2.iter().map(|row| cell(&row.get::<MVal, _>(0).unwrap_or(MVal::NULL))).collect();
    assert_eq!(asc, members(&asc_window), "SQL ascending window == ZRANGE -inf +inf BYSCORE LIMIT 4 3");
    assert_eq!(common::mysql::rows(&mut c, "SELECT bidder, amount FROM auction_bids WHERE item = 'guitar' ORDER BY amount DESC, bid_id ASC LIMIT 1").await, vec![vec![s("bid-011"), int(980)]], "SQL top bid == ZREVRANGE 0 0");
    let accepted = common::mysql::rows(&mut c, "SELECT bidder, amount FROM auction_bids WHERE item = 'guitar' ORDER BY bid_id ASC").await;
    let stream_rows: Vec<Vec<MVal>> = (0..12).map(|i| vec![s(&entries[i * 5 + 2]), int(entries[i * 5 + 4].parse().unwrap())]).collect();
    assert_eq!(accepted, stream_rows, "bid_id order == the acceptance stream");
    assert_eq!(common::mysql::ints(&mut c, "SELECT bid_id FROM auction_bids WHERE bidder = 'bid-011'").await, vec![12], "the top bid was accepted last");
    scenario_common::perf::scenario("zr132", t0);
}
