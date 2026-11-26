//! Multi-store scenario #150 -- a fantasy league desk fuses three weekly scoring
//! zsets (member = player, score = points): ZUNIONSTORE WEIGHTS 1 2 3 folds each
//! player's played weeks into ONE weighted total; ZINTERSTORE MIN / MAX keep
//! the players who played all three weeks (weights apply BEFORE the fold).
//! Argument errors are Redis's texts, a wrong-typed source OR destination is
//! refused untouched, a key outside the tag is CROSSSLOT, and the SQL mirror
//! (CASE weights, MIN / MAX behind `HAVING COUNT(*) = 3`) reads back the same
//! tables. The prize draw (ZRANDMEMBER / HRANDFIELD / SPOP) is asserted as
//! SETS, never by order.
//! SURFACES: RESP (zset + hash + set) + MySQL. One REAL node, MySQL.
//! Deviation (pinned): a MISSING source key is an EMPTY zset -- the STORE
//! succeeds with the surviving members instead of erroring (zsetops_cmd.rs:144).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, r, FrontSpec};
use std::time::Duration;

const TAG: &str = "fuse150";
/// Week weights: week 2 counts double, week 3 triple.
const WEIGHTS: [i64; 3] = [1, 2, 3];
/// The season: player -> points per week (`None` = did not play).
const SEASON: [(&str, [Option<i64>; 3]); 6] = [
    ("adal", [Some(12), Some(7), Some(9)]),
    ("bree", [Some(15), None, Some(4)]),
    ("cora", [None, Some(8), Some(11)]),
    ("dmitri", [Some(3), Some(6), None]),
    ("elena", [Some(20), Some(14), Some(18)]),
    ("farid", [None, Some(2), None]),
];
/// Club of every roster player (the roster hash HRANDFIELD samples).
const CLUBS: [(&str, &str); 6] = [
    ("adal", "lions"),
    ("bree", "lions"),
    ("cora", "hawks"),
    ("dmitri", "hawks"),
    ("elena", "owls"),
    ("farid", "owls"),
];
/// Prize-draw entrants (the ticket set SPOP drains).
const TICKET: [&str; 5] = ["adal", "bree", "cora", "elena", "farid"];
const WEEKS: [&str; 3] = ["wk1", "wk2", "wk3"];
const WRONGTYPE: &str = "-WRONGTYPE Operation against a key holding the wrong kind of value";
const CROSSSLOT: &str = "-ERR CROSSSLOT Keys in request don't hash to the same slot";
const NOT_INT: &str = "-ERR value is not an integer or out of range";
const SYNTAX: &str = "-ERR syntax error";

fn key(name: &str) -> String {
    format!("{{{TAG}}}{name}")
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
    common::lite::text(
        &common::lite::cmd_full_reply_wait(addr, common::TOKEN, &argv(args), 25, 10_000).await,
    )
}

/// Wire-order cells of a full-drain reply: `:n` -> "n", `$len` -> the
/// payload that follows (array heads skipped).
fn cells(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Some(n) = toks[i].strip_prefix(':') {
            out.push(n.to_string());
        } else if toks[i].strip_prefix('$').is_some_and(|h| h.parse::<usize>().is_ok()) {
            out.push(toks.get(i + 1).unwrap_or(&"").to_string());
            i += 1;
        }
        i += 1;
    }
    out
}

/// SQL cell as text (NULL keeps its spelling).
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

fn grid(rs: Vec<Vec<MVal>>) -> Vec<Vec<String>> {
    rs.into_iter().map(|row| row.iter().map(cell).collect()).collect()
}

/// Week `w` (0-based) of the season: `(player, points)` in season order.
fn week(w: usize) -> Vec<(&'static str, i64)> {
    SEASON.iter().filter_map(|(p, s)| s[w].map(|v| (*p, v))).collect()
}

fn add(x: i64, y: i64) -> i64 {
    x + y
}

/// The Rust-side fusion: every player (`full` = only those who played all
/// three weeks), each played week's points times its weight, folded by
/// `agg`; `(member, score)` sorted by (score, member) like ZRANGE.
fn fuse(weights: [i64; 3], full: bool, agg: fn(i64, i64) -> i64) -> Vec<(String, String)> {
    let mut out: Vec<(&str, i64)> = SEASON
        .iter()
        .filter(|(_, s)| !full || s.iter().all(Option::is_some))
        .map(|(p, s)| {
            let folded = s
                .iter()
                .zip(weights)
                .filter_map(|(v, w)| v.map(|v| v * w))
                .reduce(agg)
                .unwrap_or(0);
            (*p, folded)
        })
        .collect();
    out.sort_by_key(|&(p, v)| (v, p));
    out.into_iter().map(|(p, v)| (p.to_string(), v.to_string())).collect()
}

/// Flat `[m, s, m, s]` wire cells of a `(member, score)` table.
fn flat(pairs: &[(String, String)]) -> Vec<String> {
    pairs.iter().flat_map(|(m, s)| [m.clone(), s.clone()]).collect()
}

/// Flat `[a, b, a, b]` cells -> the `(a, b)` pairs, sorted (order-free).
fn sorted_pairs(flat: Vec<String>) -> Vec<(String, String)> {
    assert_eq!(flat.len() % 2, 0, "pairs arrive interleaved: {flat:?}");
    let mut pairs: Vec<(String, String)> = flat
        .chunks(2)
        .map(|c| {
            (
                c.first().cloned().unwrap_or_default(),
                c.get(1).cloned().unwrap_or_default(),
            )
        })
        .collect();
    pairs.sort();
    pairs
}

#[tokio::test]
async fn league_fusion_weights_aggregates_and_draws() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { mysql: true, ..Default::default() };
    let (front, a) = front_world(TAG, &spec).await;

    // a) The Rust-side season tables (the oracle for every reply below).
    let total = fuse(WEIGHTS, false, add);
    let plain = fuse([1, 1, 1], false, add);
    let floor = fuse([1, 1, 1], true, i64::min);
    let ceil = fuse([1, 1, 1], true, i64::max);
    let floor_w = fuse(WEIGHTS, true, i64::min);
    let ceil_w = fuse(WEIGHTS, true, i64::max);
    assert_eq!(
        flat(&total),
        ["farid", "4", "dmitri", "15", "bree", "27", "cora", "49", "adal", "53", "elena", "102"],
        "model: adal 12+2*7+3*9 = 53, elena 20+28+54 = 102, farid 2*2"
    );
    assert_eq!(
        [flat(&floor).join(","), flat(&ceil).join(","), flat(&floor_w).join(","), flat(&ceil_w).join(",")],
        ["adal,7,elena,14", "adal,12,elena,20", "adal,12,elena,20", "adal,27,elena,54"],
        "model: only adal and elena played all three weeks"
    );
    assert_ne!(floor_w, floor, "the weights fold BEFORE MIN (model)");

    // b) The three weekly zsets, the roster hash and the ticket drum.
    let wk: Vec<String> = WEEKS.iter().map(|w| key(w)).collect();
    for w in 0..3 {
        let mut owned: Vec<String> = vec!["ZADD".into(), wk[w].clone()];
        for (p, v) in week(w) {
            owned.push(v.to_string());
            owned.push(p.to_string());
        }
        let args: Vec<&[u8]> = owned.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(common::lite::text(&r(&a, &args).await), format!(":{}", week(w).len()), "ZADD {}", WEEKS[w]);
    }
    let (roster, ticket) = (key("roster"), key("ticket"));
    let mut owned: Vec<String> = vec!["HSET".into(), roster.clone()];
    for (p, club) in CLUBS {
        owned.push(p.to_string());
        owned.push(club.to_string());
    }
    let args: Vec<&[u8]> = owned.iter().map(|s| s.as_bytes()).collect();
    assert_eq!(common::lite::text(&r(&a, &args).await), ":6", "HSET roster");
    let mut owned: Vec<String> = vec!["SADD".into(), ticket.clone()];
    owned.extend(TICKET.iter().map(|p| p.to_string()));
    let args: Vec<&[u8]> = owned.iter().map(|s| s.as_bytes()).collect();
    assert_eq!(common::lite::text(&r(&a, &args).await), ":5", "SADD ticket");

    // c) Hot block -- the fusion desk: six STORE commands, each result
    //    read back with ZRANGE ... WITHSCORES and compared to the model.
    let t = scenario_common::perf::now();
    let n_total = cmd(&a, &["ZUNIONSTORE", &key("total"), "3", &wk[0], &wk[1], &wk[2], "WEIGHTS", "1", "2", "3", "AGGREGATE", "SUM"]).await;
    let got_total = cells(&drain(&a, &["ZRANGE", &key("total"), "0", "-1", "WITHSCORES"]).await);
    let n_plain = cmd(&a, &["ZUNIONSTORE", &key("plain"), "3", &wk[0], &wk[1], &wk[2]]).await;
    let got_plain = cells(&drain(&a, &["ZRANGE", &key("plain"), "0", "-1", "WITHSCORES"]).await);
    let n_floor = cmd(&a, &["ZINTERSTORE", &key("floor"), "3", &wk[0], &wk[1], &wk[2], "AGGREGATE", "MIN"]).await;
    let got_floor = cells(&drain(&a, &["ZRANGE", &key("floor"), "0", "-1", "WITHSCORES"]).await);
    let n_ceil = cmd(&a, &["ZINTERSTORE", &key("ceil"), "3", &wk[0], &wk[1], &wk[2], "AGGREGATE", "MAX"]).await;
    let got_ceil = cells(&drain(&a, &["ZRANGE", &key("ceil"), "0", "-1", "WITHSCORES"]).await);
    let n_floorw = cmd(&a, &["ZINTERSTORE", &key("floorw"), "3", &wk[0], &wk[1], &wk[2], "WEIGHTS", "1", "2", "3", "AGGREGATE", "MIN"]).await;
    let got_floorw = cells(&drain(&a, &["ZRANGE", &key("floorw"), "0", "-1", "WITHSCORES"]).await);
    let n_ceilw = cmd(&a, &["ZINTERSTORE", &key("ceilw"), "3", &wk[0], &wk[1], &wk[2], "WEIGHTS", "1", "2", "3", "AGGREGATE", "MAX"]).await;
    let got_ceilw = cells(&drain(&a, &["ZRANGE", &key("ceilw"), "0", "-1", "WITHSCORES"]).await);
    let card = cmd(&a, &["ZCARD", &key("total")]).await;
    scenario_common::perf::within("fuse150/fusion-block", t, Duration::from_secs(2));
    assert_eq!(
        [n_total.as_str(), n_plain.as_str(), n_floor.as_str(), n_ceil.as_str(), n_floorw.as_str(), n_ceilw.as_str()],
        [":6", ":6", ":2", ":2", ":2", ":2"],
        "STORE answers the size of its result"
    );
    assert_eq!(
        (got_total, got_plain, got_floor, got_ceil, got_floorw, got_ceilw),
        (flat(&total), flat(&plain), flat(&floor), flat(&ceil), flat(&floor_w), flat(&ceil_w)),
        "weighted UNION, plain UNION, MIN / MAX with and without WEIGHTS == the model"
    );
    assert_eq!(card, ":6", "a player who missed a week keeps a season total");

    // d) The real error texts; none of them creates the destination.
    let e = key("e");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e]).await, "-ERR wrong number of arguments for 'zunionstore' command");
    assert_eq!(cmd(&a, &["ZINTERSTORE", &e]).await, "-ERR wrong number of arguments for 'zinterstore' command");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "x", &wk[0]]).await, NOT_INT, "numkeys must be an integer");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "0", &wk[0]]).await, "-ERR at least 1 input key is needed for 'zunionstore' command");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "-1", &wk[0]]).await, "-ERR at least 1 input key is needed for 'zunionstore' command");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "3", &wk[0], &wk[1]]).await, "-ERR syntax error, wrong number of keys");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "2", &wk[0], &wk[1], "WEIGHTS", "1"]).await, "-ERR WEIGHTS options doesn't match the number of keys");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "2", &wk[0], &wk[1], "WEIGHTS"]).await, "-ERR WEIGHTS options doesn't match the number of keys");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "2", &wk[0], &wk[1], "WEIGHTS", "1", "x"]).await, "-ERR weight value is not a float");
    for tail in [&["AGGREGATE"][..], &["AGGREGATE", "MEDIAN"], &["BOGUS"]] {
        let mut args = vec!["ZUNIONSTORE", e.as_str(), "2", wk[0].as_str(), wk[1].as_str()];
        args.extend_from_slice(tail);
        assert_eq!(cmd(&a, &args).await, SYNTAX, "options {tail:?}");
    }
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "2", &wk[0], &roster]).await, WRONGTYPE, "a hash SOURCE");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &roster, "1", &wk[0]]).await, WRONGTYPE, "a hash DESTINATION");
    assert_eq!(cmd(&a, &["HLEN", &roster]).await, ":6", "the refused STORE left the destination intact");
    assert_eq!(cmd(&a, &["ZUNIONSTORE", &e, "2", &wk[0], "{fuse150x}wk2"]).await, CROSSSLOT);
    assert_eq!(cmd(&a, &["EXISTS", &e]).await, ":0", "no failed STORE created its destination");

    // e) Deviation: a missing source is an empty zset, not an error.
    let n_ghost = cmd(&a, &["ZUNIONSTORE", &key("ghost"), "2", &wk[0], &key("nosuch")]).await;
    let mut got_ghost = cells(&drain(&a, &["ZRANGE", &key("ghost"), "0", "-1"]).await);
    got_ghost.sort();
    let mut want_ghost: Vec<String> = week(0).iter().map(|(p, _)| p.to_string()).collect();
    want_ghost.sort();
    assert_eq!(n_ghost, ":4", "the missing week contributes nothing");
    assert_eq!(got_ghost, want_ghost);
    assert_eq!(cmd(&a, &["ZINTERSTORE", &key("ghostint"), "2", &wk[0], &key("nosuch")]).await, ":0", "nobody is in every source");
    assert_eq!(cmd(&a, &["EXISTS", &key("ghostint")]).await, ":0", "an empty result leaves no key");

    // f) The prize draw: random picks are checked as SETS.
    let total_sorted = sorted_pairs(flat(&total));
    let two = sorted_pairs(cells(&drain(&a, &["ZRANDMEMBER", &key("total"), "2", "WITHVALUES"]).await));
    assert_eq!(two.len(), 2, "count 2 draws two members");
    assert_ne!(two[0].0, two[1].0, "a positive count draws DISTINCT members");
    assert!(two.iter().all(|p| total_sorted.contains(p)), "each pair is (member, season total): {two:?}");
    assert_eq!(sorted_pairs(cells(&drain(&a, &["ZRANDMEMBER", &key("total"), "10", "WITHVALUES"]).await)), total_sorted, "past the cardinality: every member once");
    let mut clubs: Vec<(String, String)> = CLUBS.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect();
    clubs.sort();
    let htwo = sorted_pairs(cells(&drain(&a, &["HRANDFIELD", &roster, "2", "WITHVALUES"]).await));
    assert_eq!(htwo.len(), 2);
    assert_ne!(htwo[0].0, htwo[1].0, "distinct fields");
    assert!(htwo.iter().all(|p| clubs.contains(p)), "each pair is (player, club): {htwo:?}");
    assert_eq!(sorted_pairs(cells(&drain(&a, &["HRANDFIELD", &roster, "10", "WITHVALUES"]).await)), clubs, "past the field count: every pair once");
    assert_eq!(cmd(&a, &["ZRANDMEMBER", &key("total"), "2", "BOGUS"]).await, SYNTAX);
    assert_eq!(cmd(&a, &["ZRANDMEMBER", &key("total"), "x"]).await, NOT_INT);
    assert_eq!(cmd(&a, &["HRANDFIELD", &roster, "2", "BOGUS"]).await, SYNTAX);

    let mut popped = cells(&drain(&a, &["SPOP", &ticket, "2"]).await);
    popped.sort();
    let entrants: Vec<String> = TICKET.iter().map(|p| p.to_string()).collect();
    assert_eq!(popped.len(), 2, "two winners drawn");
    assert_ne!(popped[0], popped[1], "distinct winners");
    assert!(popped.iter().all(|m| entrants.contains(m)), "winners come from the drum: {popped:?}");
    let rest: Vec<String> = entrants.iter().filter(|m| !popped.contains(m)).cloned().collect();
    assert_eq!(cmd(&a, &["SCARD", &ticket]).await, ":3", "the drawn members left the drum");
    let mut left = cells(&drain(&a, &["SMEMBERS", &ticket]).await);
    left.sort();
    assert_eq!(left, rest, "exactly the drawn members left");
    for bad in ["0", "-1"] {
        assert_eq!(cmd(&a, &["SPOP", &ticket, bad]).await, "-ERR value is out of range, must be positive", "count {bad}");
    }
    assert_eq!(cmd(&a, &["SPOP", &ticket, "x"]).await, NOT_INT);
    assert_eq!(cmd(&a, &["SCARD", &ticket]).await, ":3", "the refused SPOPs drew nothing");
    let mut drum = cells(&drain(&a, &["SPOP", &ticket, "5"]).await);
    drum.sort();
    assert_eq!(drum, rest, "a count past the cardinality empties the drum");
    assert_eq!(cmd(&a, &["SCARD", &ticket]).await, ":0");
    assert_eq!(cmd(&a, &["SPOP", &ticket]).await, "$-1", "an empty drum draws nil");

    // g) The SQL mirror of the same thirteen played weeks (one row per
    //    played week: the plane requires a PRIMARY KEY -> (player, week)).
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE league_scores (player VARCHAR(16) NOT NULL, week TINYINT NOT NULL, score INT NOT NULL, PRIMARY KEY (player, week))").await;
    let values: Vec<String> = SEASON
        .iter()
        .flat_map(|(p, s)| s.iter().enumerate().filter_map(move |(w, v)| v.map(|v| format!("('{p}',{}, {})", w + 1, v))))
        .collect();
    let played: usize = SEASON.iter().map(|(_, s)| s.iter().flatten().count()).sum();
    assert_eq!((played, values.len()), (13, 13), "six players, 3+2+2+2+3+1 = thirteen played weeks");
    run(&mut c, &format!("INSERT INTO league_scores (player, week, score) VALUES {}", values.join(","))).await;
    let mut by_player: Vec<(String, i64)> = total.iter().map(|(p, s)| (p.clone(), s.parse::<i64>().unwrap_or(i64::MIN))).collect();
    by_player.sort();
    let want: Vec<Vec<String>> = by_player.iter().map(|(p, s)| vec![p.clone(), s.to_string()]).collect();
    let got = grid(rows(&mut c, "SELECT player, SUM(score * CASE week WHEN 1 THEN 1 WHEN 2 THEN 2 ELSE 3 END) AS weighted FROM league_scores GROUP BY player ORDER BY player").await);
    assert_eq!(got, want, "simple CASE weight == the ZUNIONSTORE WEIGHTS 1 2 3 table");
    let week_sum = |w: usize| -> i64 { week(w).iter().map(|(_, v)| v).sum() };
    let weeks: Vec<Vec<String>> = (0..3).map(|w| vec![(w + 1).to_string(), (week_sum(w) * WEIGHTS[w]).to_string()]).collect();
    let got = grid(rows(&mut c, "SELECT week, SUM(score * CASE WHEN week = 1 THEN 1 WHEN week = 2 THEN 2 ELSE 3 END) AS weighted FROM league_scores GROUP BY week ORDER BY week").await);
    assert_eq!(got, weeks, "searched CASE: per-week weighted points");
    let grand: i64 = (0..3).map(|w| week_sum(w) * WEIGHTS[w]).sum();
    let unweighted: i64 = (0..3).map(week_sum).sum();
    let table_sum = |table: &[(String, String)]| -> i64 { table.iter().map(|(_, s)| s.parse::<i64>().unwrap_or(i64::MIN)).sum() };
    assert_eq!((table_sum(&total), table_sum(&plain)), (grand, unweighted), "model: the fused tables sum to the league points");
    let got = grid(rows(&mut c, "SELECT SUM(score * CASE week WHEN 1 THEN 1 WHEN 2 THEN 2 ELSE 3 END), SUM(score) FROM league_scores").await);
    assert_eq!(got, vec![vec![grand.to_string(), unweighted.to_string()]], "league-wide weighted and raw points");
    let both: Vec<Vec<String>> = floor.iter().zip(&ceil).map(|((p, lo), (_, hi))| vec![p.clone(), lo.clone(), hi.clone()]).collect();
    let got = grid(rows(&mut c, "SELECT player, MIN(score), MAX(score) FROM league_scores GROUP BY player HAVING COUNT(*) = 3 ORDER BY player").await);
    assert_eq!(got, both, "MIN / MAX behind HAVING COUNT(*) = 3 == the floor / ceiling tables");
    scenario_common::perf::scenario("fuse150", t0);
}
