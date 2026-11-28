//! Multi-store scenario #127 -- lite queue hygiene on one REAL node with
//! BOTH background sweeps armed at 300ms (`lite.redelivery_idle_ms` /
//! `lite.consumer_gc_ms`), reconciled into a SQL table: an abandoned
//! delivery is re-handed by the idle sweep (XPENDING deliveries 1 ->
//! \>= 2 with no client anywhere near it, < 3s), a MAXDELIVERY 2 group
//! with an explicit DLQ target dead-letters the message driven past its
//! cap (XLEN dlq == 1, the entry carries the payload + trace fields,
//! the PEL row is gone) while XACK clears its sibling from the PEL, and
//! a consumer with an empty PEL vanishes from XINFO CONSUMERS (< 3s).
//! One REAL node, RESP + MySQL.

use crate::common;
use crate::scenario_common;
use common::lite::{pel_rows, pending_rows};
use common::mysql::{connect_root, ddl, int, ints, one, rows as sql_rows, run};
use mysql_async::Value as MVal;
use scenario_common::{FrontSpec, front_world, int_of, r, rb, rf, rt};
use std::time::{Duration, Instant};

const IDLE_MS: u64 = 300;
const RD: &[u8] = b"gc127/rd"; // idle-redelivery stream
const DL: &[u8] = b"gc127/dl"; // MAXDELIVERY + DLQ stream
const DEAD: &[u8] = b"gc127/dead"; // explicit DLQ target (same parent = same slot)
const GC: &[u8] = b"gc127/gc"; // consumer-GC stream
/// Ceiling for the consumer-GC wait (1s sweep tick; a loaded host
/// stretched it past 3s in the batch-14 gate).
const GC_BUDGET: Duration = Duration::from_secs(10);
const PEL0: &str = "*3\r\n:0\r\n$-1\r\n$-1\r\n";

fn spec() -> FrontSpec {
    FrontSpec {
        mysql: true,
        extra_yaml: format!(
            "lite:\n  redelivery_idle_ms: {IDLE_MS}\n  consumer_gc_ms: {IDLE_MS}\n"
        ),
        ..Default::default()
    }
}

/// Consumer names of `XINFO CONSUMERS <s> <g>`, sorted (positional
/// tokens: each bulk value sits two tokens after its `name` label).
/// The array-of-arrays reply rides the quiet-gap reader with a short
/// gap, so a poll costs ~100ms instead of the fixtures' 250ms.
async fn consumers(a: &str, s: &[u8], g: &[u8]) -> Vec<String> {
    let raw = common::lite::cmd_full_reply_wait(
        a,
        common::TOKEN,
        &[b"XINFO", b"CONSUMERS", s, g],
        100,
        10_000,
    )
    .await;
    let reply = String::from_utf8_lossy(&raw).into_owned();
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let mut names: Vec<String> = toks
        .windows(3)
        .filter(|w| w[0] == "name")
        .map(|w| w[2].to_string())
        .collect();
    names.sort();
    names
}

/// Deliveries of one pending id (0 when the row left the PEL), drained
/// through a 20ms quiet gap (the fixture's `pending_rows` parks 200ms):
/// the "still exactly 1" probe below is read ~25ms after the delivery,
/// while the idle sweep first counts the row from 300ms on its next
/// 200ms tick (`src/lite/redeliver.rs`) -- a 250ms reader here would
/// leave the probe ~45ms from that window.
async fn deliveries_of(a: &str, s: &[u8], g: &[u8], id: &str) -> u64 {
    let reply = common::lite::cmd_full_reply_wait(
        a,
        common::TOKEN,
        &[b"xpending", s, g, b"-", b"+", b"10"],
        20,
        10_000,
    )
    .await;
    pel_rows(&reply)
        .into_iter()
        .filter(|r| r.len() > 5)
        .find(|r| r[1] == id)
        .map(|r| r[5].trim_start_matches(':').parse().unwrap_or(0))
        .unwrap_or(0)
}

/// XADD with an explicit id; asserts the echoed id.
async fn xadd(a: &str, s: &[u8], id: &str, v: &str) {
    let reply = rb(a, &[b"XADD", s, id.as_bytes(), b"f", v.as_bytes()]).await;
    assert_eq!(reply, id, "xadd {id}");
}

#[tokio::test]
async fn idle_redelivery_dlq_and_consumer_gc_reconcile() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let (front, a) = front_world("gc127", &spec()).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE gc127_reconcile (id INT PRIMARY KEY, stream VARCHAR(40) NOT NULL, \
         grp VARCHAR(24) NOT NULL, consumer VARCHAR(24) NOT NULL, deliveries INT NOT NULL, \
         in_dlq INT NOT NULL, acked INT NOT NULL, collected INT NOT NULL)",
    )
    .await;

    // a) Idle redelivery: XADD + XGROUP CREATE + one `>` delivery to c1
    //    that then goes silent. The row sits at exactly 1 delivery, and
    //    the background sweep (idle threshold 300ms, 200ms ticks) raises
    //    it without any client touching the stream.
    xadd(&a, RD, "1-1", "rd-payload").await;
    assert_eq!(
        rt(&a, &[b"XGROUP", b"CREATE", RD, b"rdg", b"0-0"]).await,
        "+OK"
    );
    // The delivery itself is read through the 20ms gap too: the
    // fixture's `rf` would park 250ms, leaving the "still exactly 1"
    // probe below ~45ms before the idle sweep's first window. The
    // redelivery clock is stamped BEFORE the delivery, so the lower
    // bound below is measured conservatively.
    let t = scenario_common::perf::now();
    let read = common::lite::text(
        &common::lite::cmd_full_reply_wait(
            &a,
            common::TOKEN,
            &[b"XREADGROUP", b"GROUP", b"rdg", b"c1", b"STREAMS", RD, b">"],
            20,
            10_000,
        )
        .await,
    );
    assert!(
        read.contains("1-1") && read.contains("rd-payload"),
        "delivery: {read}"
    );
    assert_eq!(deliveries_of(&a, RD, b"rdg", "1-1").await, 1, "delivery #1");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut rd_times;
    loop {
        rd_times = deliveries_of(&a, RD, b"rdg", "1-1").await;
        assert!(
            rd_times > 0,
            "nothing should ack or dead-letter the idle row"
        );
        if rd_times >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sweep never re-handed the idle row within 3s (times={rd_times}); {}",
            front.node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let redelivered = scenario_common::perf::within("gc127/redelivery", t, Duration::from_secs(3));
    assert!(
        redelivered >= Duration::from_millis(IDLE_MS - 50),
        "a row re-handed before its idle threshold would be a bug: {redelivered:?}"
    );
    // Ownership never moved: the sweep re-hands to the CURRENT consumer
    // (the count may already have risen further while we read it).
    let rows = pending_rows(&a, RD, b"rdg", 10).await;
    assert_eq!(rows.len(), 1, "one pending row: {rows:?}");
    assert_eq!(rows[0].0, "1-1");
    assert_eq!(rows[0].1, "c1", "redelivery keeps the owner");
    assert!(rows[0].2 >= rd_times, "count never falls: {rows:?}");

    // b) MAXDELIVERY 2 + explicit DLQ target: `>` delivery #1 to c1 for
    //    both messages, the sibling is XACKed straight away (its PEL row
    //    leaves), then the victim is claimed until the attempt that would
    //    EXCEED the cap transfers it to the DLQ stream.
    xadd(&a, DL, "1-1", "victim-payload").await;
    xadd(&a, DL, "1-2", "sibling-payload").await;
    assert_eq!(
        rt(
            &a,
            &[
                b"XGROUP",
                b"CREATE",
                DL,
                b"dlg",
                b"0-0",
                b"MAXDELIVERY",
                b"2",
                b"DLQ",
                DEAD
            ]
        )
        .await,
        "+OK"
    );
    let read = rf(
        &a,
        &[b"XREADGROUP", b"GROUP", b"dlg", b"c1", b"STREAMS", DL, b">"],
    )
    .await;
    assert!(
        read.contains("1-1") && read.contains("1-2") && read.contains("victim-payload"),
        "both delivered: {read}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XACK", DL, b"dlg", b"1-2"]).await),
        1,
        "ack sibling"
    );
    assert_eq!(
        pending_rows(&a, DL, b"dlg", 10)
            .await
            .iter()
            .map(|(i, _, _)| i.clone())
            .collect::<Vec<_>>(),
        vec!["1-1".to_string()],
        "XACK cleared the sibling's PEL row"
    );
    // Claims to c2: each either DELIVERS the entry (still under the cap)
    // or returns an empty array once the row transferred. The idle sweep
    // may land a bump in between, so the delivering count is 0..2 -- but
    // the end state is fixed: exactly one DLQ entry, nothing pending.
    let mut claim_replies = Vec::new();
    for _ in 0..4 {
        let reply = rf(&a, &[b"XCLAIM", DL, b"dlg", b"c2", b"0", b"1-1"]).await;
        let transfers = reply == "*0\r\n";
        assert!(
            transfers || (reply.contains("1-1") && reply.contains("victim-payload")),
            "claim delivers or transfers: {reply}"
        );
        claim_replies.push(reply);
        if int_of(&r(&a, &[b"XLEN", DEAD]).await) == 1 {
            break;
        }
    }
    // At most ONE claim can deliver (1 -> 2 reaches the cap; the next
    // hand-out, mine or the sweep's, transfers).
    assert!(
        claim_replies
            .iter()
            .filter(|r| r.as_str() != "*0\r\n")
            .count()
            <= 1,
        "claims: {claim_replies:?}"
    );
    assert_eq!(int_of(&r(&a, &[b"XLEN", DEAD]).await), 1, "one dead letter");
    assert_eq!(
        rf(&a, &[b"XPENDING", DL, b"dlg"]).await,
        PEL0,
        "victim left the PEL"
    );
    let dead = rf(&a, &[b"XRANGE", DEAD, b"-", b"+"]).await;
    for needle in [
        "1-1",
        "victim-payload",
        "__dlq_group",
        "__dlq_times",
        "__dlq_src",
    ] {
        assert!(dead.contains(needle), "DLQ entry carries {needle}: {dead}");
    }
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", DL]).await),
        2,
        "source keeps its history"
    );
    // The transferred id never resurfaces through `>`. c9 acks its own
    // delivery at once, so 1-3 never idles into the sweep and the DLQ
    // depth stays at exactly the one dead letter observed above.
    xadd(&a, DL, "1-3", "after-payload").await;
    let again = rf(
        &a,
        &[b"XREADGROUP", b"GROUP", b"dlg", b"c9", b"STREAMS", DL, b">"],
    )
    .await;
    assert!(
        again.contains("1-3") && !again.contains("1-1") && !again.contains("1-2"),
        "post-transfer `>`: {again}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XACK", DL, b"dlg", b"1-3"]).await),
        1,
        "c9 acks 1-3"
    );
    assert_eq!(
        rf(&a, &[b"XPENDING", DL, b"dlg"]).await,
        PEL0,
        "PEL empty again"
    );

    // c) Consumer GC: a registered consumer with an EMPTY PEL (no
    //    delivery, no lease) is collected by the background loop within
    //    the 1s tick rhythm + the 300ms idle threshold. Budget 10s, not
    //    ~2 ticks: a loaded shared host stretched the 1s tick past 3s
    //    (batch-14 gate, scenario target 302s); only the ceiling moves.
    xadd(&a, GC, "1-1", "gc-payload").await;
    assert_eq!(
        rt(&a, &[b"XGROUP", b"CREATE", GC, b"gcg", b"0-0"]).await,
        "+OK"
    );
    let t = scenario_common::perf::now();
    assert_eq!(
        int_of(&r(&a, &[b"XGROUP", b"CREATECONSUMER", GC, b"gcg", b"ghost"]).await),
        1,
        "ghost registered"
    );
    assert_eq!(
        consumers(&a, GC, b"gcg").await,
        vec!["ghost".to_string()],
        "listed first"
    );
    let deadline = Instant::now() + GC_BUDGET;
    loop {
        let roster = consumers(&a, GC, b"gcg").await;
        if roster.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "empty-PEL consumer never collected within {GC_BUDGET:?}: {roster:?}; {}",
            front.node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    scenario_common::perf::within("gc127/consumer-gc", t, GC_BUDGET);
    // A PEL holder in the same window is NOT collectable: rdg/c1 still
    // owns its re-delivered row.
    assert_eq!(
        consumers(&a, RD, b"rdg").await,
        vec!["c1".to_string()],
        "PEL holder survives"
    );
    assert!(
        deliveries_of(&a, RD, b"rdg", "1-1").await >= rd_times,
        "idle row keeps rising"
    );

    // d) SQL reconciliation: one row per observed outcome, then the
    //    invariants the story claims, checked by aggregation.
    let sql = format!(
        "INSERT INTO gc127_reconcile VALUES \
         (1, 'gc127/rd', 'rdg', 'c1', {rd_times}, 0, 0, 0), \
         (2, 'gc127/dl', 'dlg', 'c1', 0, 1, 0, 0), \
         (3, 'gc127/dl', 'dlg', 'c1', 0, 0, 1, 0), \
         (4, 'gc127/gc', 'gcg', 'ghost', 0, 0, 0, 1)"
    );
    run(&mut c, &sql).await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM gc127_reconcile").await,
        vec![4]
    );
    let sums = sql_rows(
        &mut c,
        "SELECT SUM(in_dlq), SUM(acked), SUM(collected) FROM gc127_reconcile",
    )
    .await;
    assert_eq!(
        sums,
        vec![vec![int(1), int(1), int(1)]],
        "one dead letter, one ack, one collected"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM gc127_reconcile WHERE deliveries >= 2"
        )
        .await,
        vec![1],
        "exactly one row was re-delivered"
    );
    // The DLQ depth in SQL is the live stream depth on the RESP plane.
    let live_dlq = int_of(&r(&a, &[b"XLEN", DEAD]).await);
    assert_eq!(
        one(&mut c, "SELECT SUM(in_dlq) FROM gc127_reconcile").await,
        MVal::Bytes(live_dlq.to_string().into_bytes()),
        "SQL dlq tally == XLEN dlq"
    );
    scenario_common::perf::scenario("gc127", t0);
}
