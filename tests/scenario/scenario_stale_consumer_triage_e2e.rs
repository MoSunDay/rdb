//! Multi-store scenario #147 -- stale-consumer triage on a support ticket
//! queue: eight tickets XADD'ed with explicit ids on `tickets/q0`, the
//! `triage` group rations 1-1..4-4 to alice and 5-5 to bob, XPENDING's
//! summary frame is rebuilt byte-for-byte Rust-side, XCLAIM hands alice's
//! tickets to carol (IDLE / TIME / JUSTID RETRYCOUNT hints, FORCE minting
//! a logged-but-undelivered id), MySQL's claim ledger totals equal the
//! XPENDING summary, XAUTOCLAIM sweeps the PEL page by page for erin, and
//! XACK drains it to [0, nil, nil] with committed-id 7-7.
//! Streams (XPENDING/XCLAIM/XAUTOCLAIM/XACK/XINFO) + MySQL. One REAL node, fronts mysql (RESP always).
//! Deviation: XPENDING's range grammar has NO `COUNT` keyword (src/lite/pending.rs:126-171),
//! XINFO GROUPS is a 13-element flat array without pending/lag fields (src/lite/info.rs:56-100),
//! and "PEL head only" claiming is an ORDERED-group rule (src/lite/claim.rs:281-283).

use crate::common;
use crate::scenario_common;
use common::lite::pel_rows;
use common::mysql::{connect_root, ddl, rows as sql_rows, run};
use mysql_async::Value as MVal;
use scenario_common::{bulk_at, front_world, int_of, r, stream_ids, FrontSpec};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STREAM: &str = "tickets/q0";
const GROUP: &str = "triage";

/// Bytes of a `&str` command line.
fn argv<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Full-drain reply text with a short quiet gap (the fixture's `rf`
/// parks 250ms per call).
async fn drain(a: &str, args: &[&str]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, &argv(args), 20, 10_000).await)
}

/// One-shot reply text (status / error / single bulk).
async fn one(a: &str, args: &[&str]) -> String {
    common::lite::text(&r(a, &argv(args)).await)
}

/// One-shot reply that must be `+OK`.
async fn one_ok(a: &str, args: &[&str], what: &str) {
    let reply = one(a, args).await;
    assert!(reply.starts_with("+OK"), "{what}: {reply}");
}

/// The XPENDING summary frame, rebuilt Rust-side: `[total, min, max,
/// consumer, count, ...]` with a byte-sorted consumer section
/// (`src/lite/pending.rs::append_summary`).
fn summary_frame(min: &str, max: &str, per: &[(&str, i64)]) -> String {
    let total: i64 = per.iter().map(|(_, n)| n).sum();
    let mut out = format!("*{}\r\n:{total}\r\n", 3 + per.len() * 2);
    for id in [min, max] {
        out.push_str(&format!("${}\r\n{id}\r\n", id.len()));
    }
    for (name, n) in per {
        out.push_str(&format!("${}\r\n{name}\r\n:{n}\r\n", name.len()));
    }
    out
}

/// Consumer section of a real summary reply: `[(consumer, count)]` in
/// reply (byte) order.
fn summary_per(reply: &str) -> Vec<(String, i64)> {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let mut per = Vec::new();
    let mut i = 6; // *N, :total, $len, min, $len, max
    while i + 2 < toks.len() && toks[i].starts_with('$') {
        let n: i64 = toks[i + 2].trim_start_matches(':').parse().expect("count cell");
        per.push((toks[i + 1].to_string(), n));
        i += 3;
    }
    per
}

/// `[(id, consumer, deliveries)]` of an XPENDING range reply.
fn triage_rows(reply: &str) -> Vec<(String, String, i64)> {
    pel_rows(reply.as_bytes())
        .iter()
        .map(|row| (row[1].clone(), row[3].clone(), cell_i64(&row[5])))
        .collect()
}

/// Integer value of a `:N` reply line.
fn cell_i64(line: &str) -> i64 {
    line.trim_start_matches(':').parse().expect("integer cell")
}

/// `XCLAIM tickets/q0 triage <consumer> <min-idle> <tail...>` argv.
fn claim_args<'a>(consumer: &'a str, min_idle: &'a str, tail: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec!["XCLAIM", STREAM, GROUP, consumer, min_idle];
    args.extend_from_slice(tail);
    args
}

/// The whole PEL (XPENDING range, no filters) as `(id, consumer,
/// deliveries)` rows.
async fn pending_rows(a: &str) -> Vec<(String, String, i64)> {
    let reply = drain(a, &["XPENDING", STREAM, GROUP, "IDLE", "0", "-", "+", "10"]).await;
    triage_rows(&reply)
}

/// `(next cursor, claimed ids)` of erin's XAUTOCLAIM
/// `XAUTOCLAIM tickets/q0 triage erin 0 <tail...>`: the reply is
/// `[cursor, entries|ids, deleted-ids]` and nothing here is trimmed.
async fn autoclaim(a: &str, tail: &[&str]) -> (String, Vec<String>) {
    let mut args = vec!["XAUTOCLAIM", STREAM, GROUP, "erin", "0"];
    args.extend_from_slice(tail);
    let reply = drain(a, &args).await;
    assert!(reply.ends_with("*0\r\n"), "no deleted ids: {reply}");
    let cursor = bulk_at(reply.as_bytes(), 0);
    let head = format!("*3\r\n${}\r\n{cursor}\r\n", cursor.len());
    let (_, entries) = reply
        .split_once(head.as_str())
        .unwrap_or_else(|| panic!("cursor {cursor} leads the reply: {reply}"));
    (cursor, stream_ids(entries))
}

/// Wall clock in unix ms (the server's `TIME` hint is a unix-ms stamp).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as u64
}

/// String cells of a SQL row (every column here is NOT NULL).
fn cell_s(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("expected string cell, got {other:?}"),
    }
}

/// Integer cells arrive as bytes on the text protocol.
fn cell_i(v: &MVal) -> i64 {
    match v {
        MVal::Bytes(b) => std::str::from_utf8(b).unwrap().trim().parse().unwrap(),
        MVal::Int(i) => *i,
        other => panic!("expected int cell, got {other:?}"),
    }
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `(id, consumer, deliveries)` rows typed for assert_eq against a reply.
fn rows_of(rows: &[(&str, &str, i64)]) -> Vec<(String, String, i64)> {
    rows.iter().map(|(i, c, n)| (i.to_string(), c.to_string(), *n)).collect()
}

#[tokio::test]
async fn stale_consumer_triage_claims_and_sweeps() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("triage147", &spec).await;

    // a) Eight tickets with explicit ids on the FULL stream name: the
    //    reply is the bare id; the triage desk starts at 0-0.
    for n in 1..=8 {
        let id = format!("{n}-{n}");
        let subject = format!("ticket {n} needs a look");
        let reply = one(&a, &["XADD", STREAM, &id, "sev", "high", "subject", &subject]).await;
        assert_eq!(reply, format!("${}\r\n{id}", id.len()), "explicit id echoed");
    }
    assert_eq!(int_of(&r(&a, &argv(&["XLEN", STREAM])).await), 8, "eight tickets");
    one_ok(&a, &["XGROUP", "CREATE", STREAM, GROUP, "0-0"], "create triage").await;

    // b) Rationed delivery: alice takes four, bob the fifth.
    let alice = drain(&a, &["XREADGROUP", "GROUP", GROUP, "alice", "COUNT", "4", "STREAMS", STREAM, ">"]).await;
    assert_eq!(stream_ids(&alice), strs(&["1-1", "2-2", "3-3", "4-4"]), "alice's ration");
    let bob = drain(&a, &["XREADGROUP", "GROUP", GROUP, "bob", "COUNT", "1", "STREAMS", STREAM, ">"]).await;
    assert_eq!(stream_ids(&bob), strs(&["5-5"]), "bob's ration");

    // c) XPENDING: the summary frame byte-for-byte, the range rows with
    //    their idle bracket and fresh counters, the consumer filter, and
    //    the idle floor that nothing reaches yet.
    let sum = drain(&a, &["XPENDING", STREAM, GROUP]).await;
    assert_eq!(sum, summary_frame("1-1", "5-5", &[("alice", 4), ("bob", 1)]), "summary frame");
    let all = drain(&a, &["XPENDING", STREAM, GROUP, "IDLE", "0", "-", "+", "10"]).await;
    assert_eq!(
        triage_rows(&all),
        rows_of(&[
            ("1-1", "alice", 1),
            ("2-2", "alice", 1),
            ("3-3", "alice", 1),
            ("4-4", "alice", 1),
            ("5-5", "bob", 1),
        ]),
        "five pending rows, one delivery each"
    );
    for row in pel_rows(all.as_bytes()) {
        let idle = cell_i64(&row[4]);
        assert!((0..60_000).contains(&idle), "fresh idle {idle}: {row:?}");
    }
    assert_eq!(
        one(&a, &["XPENDING", STREAM, GROUP, "-", "+", "COUNT", "10"]).await,
        "-ERR value is not an integer or out of range",
        "no COUNT keyword: the count is positional"
    );
    let mine = drain(&a, &["XPENDING", STREAM, GROUP, "-", "+", "10", "alice"]).await;
    assert_eq!(stream_ids(&mine), strs(&["1-1", "2-2", "3-3", "4-4"]), "consumer filter");
    assert_eq!(
        drain(&a, &["XPENDING", STREAM, GROUP, "IDLE", "3600000", "-", "+", "10"]).await,
        "*0\r\n",
        "nothing idle for an hour"
    );

    // d) Triage: carol takes alice's tickets over (IDLE backdates the
    //    delivery clock, TIME pins it, JUSTID moves ownership only and
    //    RETRYCOUNT rewrites the counter), FORCE mints a row for a
    //    logged-but-undelivered ticket, and the refusals stay empty.
    let t = scenario_common::perf::now();
    let c1 = drain(&a, &claim_args("carol", "0", &["1-1", "2-2", "IDLE", "60000"])).await;
    assert_eq!(stream_ids(&c1), strs(&["1-1", "2-2"]), "plain claim, entries echoed");
    let backdated = (now_ms() - 120_000).to_string();
    let c2 = drain(&a, &claim_args("carol", "0", &["3-3", "TIME", &backdated])).await;
    assert_eq!(stream_ids(&c2), strs(&["3-3"]), "TIME claim");
    assert_eq!(
        drain(&a, &claim_args("carol", "0", &["4-4", "JUSTID", "RETRYCOUNT", "7"])).await,
        "*1\r\n$3\r\n4-4\r\n",
        "JUSTID replies ids only"
    );
    let c4 = drain(&a, &claim_args("carol", "0", &["7-7", "FORCE"])).await;
    assert_eq!(stream_ids(&c4), strs(&["7-7"]), "FORCE mints a pending row");
    assert_eq!(drain(&a, &claim_args("carol", "0", &["99-0", "FORCE"])).await, "*0\r\n", "FORCE cannot mint a trimmed id");
    assert_eq!(drain(&a, &claim_args("carol", "0", &["6-6"])).await, "*0\r\n", "no FORCE: an undelivered id is skipped");
    assert_eq!(drain(&a, &claim_args("dave", "3600000", &["5-5"])).await, "*0\r\n", "bob's fresh row stays with bob");
    scenario_common::perf::within("triage147/7-claims", t, Duration::from_secs(2));

    // e) The PEL after the claims: the summary, the owners + counters
    //    the hints produced, the idle brackets, and the 90s floor that
    //    names only the TIME-backdated ticket.
    let sum = drain(&a, &["XPENDING", STREAM, GROUP]).await;
    assert_eq!(sum, summary_frame("1-1", "7-7", &[("bob", 1), ("carol", 5)]), "post-claim summary");
    let claimed = drain(&a, &["XPENDING", STREAM, GROUP, "IDLE", "0", "-", "+", "10"]).await;
    assert_eq!(
        triage_rows(&claimed),
        rows_of(&[
            ("1-1", "carol", 2),
            ("2-2", "carol", 2),
            ("3-3", "carol", 2),
            ("4-4", "carol", 7),
            ("5-5", "bob", 1),
            ("7-7", "carol", 1),
        ]),
        "owners + delivery counters after the hints"
    );
    for row in pel_rows(claimed.as_bytes()) {
        let floor = match row[1].as_str() {
            "1-1" | "2-2" => 60_000,
            "3-3" => 120_000,
            _ => 0,
        };
        let idle = cell_i64(&row[4]);
        assert!((floor..floor + 30_000).contains(&idle), "idle {idle} of {row:?}");
    }
    let stale = drain(&a, &["XPENDING", STREAM, GROUP, "IDLE", "90000", "-", "+", "10"]).await;
    assert_eq!(stream_ids(&stale), strs(&["3-3"]), "90s floor: only the TIME-backdated ticket");

    // f) MySQL keeps the claim ledger: its per-consumer totals equal the
    //    XPENDING summary's consumer section.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE triage_claims (ticket_id VARCHAR(16) NOT NULL PRIMARY KEY, \
         consumer VARCHAR(16) NOT NULL)",
    )
    .await;
    let mut values = String::new();
    for (i, (id, who, _)) in triage_rows(&claimed).iter().enumerate() {
        values.push_str(&format!("{}('{id}','{who}')", if i > 0 { "," } else { "" }));
    }
    run(&mut c, &format!("INSERT INTO triage_claims (ticket_id, consumer) VALUES {values}")).await;
    let ledger: Vec<(String, i64)> = sql_rows(
        &mut c,
        "SELECT consumer, COUNT(*) FROM triage_claims GROUP BY consumer ORDER BY consumer",
    )
    .await
    .iter()
    .map(|r| (cell_s(&r[0]), cell_i(&r[1])))
    .collect();
    assert_eq!(ledger, summary_per(&sum), "ledger totals == XPENDING consumer section");
    let carol: Vec<String> = sql_rows(
        &mut c,
        "SELECT ticket_id FROM triage_claims WHERE consumer = 'carol' ORDER BY ticket_id",
    )
        .await
        .iter()
        .map(|r| cell_s(&r[0]))
        .collect();
    assert_eq!(carol, strs(&["1-1", "2-2", "3-3", "4-4", "7-7"]), "carol's ledger page");

    // g) XAUTOCLAIM sweeps the PEL for erin, COUNT 2 at a time: the
    //    cursor is the successor of the last scanned id, the page past
    //    the end comes back empty with cursor 0-0, a restart from 0-0
    //    re-claims (min-idle 0 always passes), and JUSTID moves rows
    //    without touching the delivery counters.
    let t = scenario_common::perf::now();
    assert_eq!(autoclaim(&a, &["0-0", "COUNT", "2"]).await, ("2-3".to_string(), strs(&["1-1", "2-2"])), "page 1");
    assert_eq!(autoclaim(&a, &["2-3", "COUNT", "2"]).await, ("4-5".to_string(), strs(&["3-3", "4-4"])), "page 2");
    assert_eq!(
        autoclaim(&a, &["4-5", "COUNT", "2"]).await,
        ("0-0".to_string(), strs(&["5-5", "7-7"])),
        "page 3 runs off the end"
    );
    assert_eq!(autoclaim(&a, &["7-8", "COUNT", "2"]).await, ("0-0".to_string(), Vec::new()), "past the end: empty page");
    let all_ids = strs(&["1-1", "2-2", "3-3", "4-4", "5-5", "7-7"]);
    assert_eq!(
        autoclaim(&a, &["0-0", "COUNT", "10"]).await,
        ("0-0".to_string(), all_ids.clone()),
        "restart from 0-0 re-claims every row"
    );
    let before = pending_rows(&a).await;
    assert_eq!(autoclaim(&a, &["0-0", "COUNT", "10", "JUSTID"]).await, ("0-0".to_string(), all_ids), "JUSTID sweep: ids only");
    let after = pending_rows(&a).await;
    assert_eq!(before, after, "JUSTID leaves owners and counters untouched");
    assert_eq!(
        after,
        rows_of(&[
            ("1-1", "erin", 4),
            ("2-2", "erin", 4),
            ("3-3", "erin", 4),
            ("4-4", "erin", 9),
            ("5-5", "erin", 3),
            ("7-7", "erin", 3),
        ]),
        "three sweeps + one restart: every row is erin's"
    );
    scenario_common::perf::within("triage147/6-sweeps+2-pending", t, Duration::from_secs(2));

    // h) XACK drains the PEL: 6 ids beyond the 0-0 watermark, the empty
    //    summary [0, nil, nil], and the committed-id promoted to 7-7.
    let acked = drain(&a, &["XACK", STREAM, GROUP, "1-1", "2-2", "3-3", "4-4", "5-5", "7-7"]).await;
    assert_eq!(acked, ":6\r\n", "six acks");
    assert_eq!(drain(&a, &["XPENDING", STREAM, GROUP]).await, "*3\r\n:0\r\n$-1\r\n$-1\r\n", "empty summary");
    assert_eq!(drain(&a, &["XPENDING", STREAM, GROUP, "-", "+", "10"]).await, "*0\r\n", "empty range");
    let groups = drain(&a, &["XINFO", "GROUPS", STREAM]).await;
    assert!(groups.starts_with("*1\r\n*13\r\n$6\r\ntriage\r\n"), "13-element flat group: {groups}");
    assert!(
        groups.contains("$17\r\nlast-delivered-id\r\n$3\r\n7-7\r\n$12\r\ncommitted-id\r\n$3\r\n7-7\r\n"),
        "committed-id promoted: {groups}"
    );
    assert_eq!(int_of(&r(&a, &argv(&["XLEN", STREAM])).await), 8, "acks never trim the log");
    scenario_common::perf::scenario("triage147", t0);
}
