//! Lite auto-redelivery e2e (in-process, deterministic clock): the
//! sweep design of plans/2026-10-06-mq-gap 01-engine-reliability.md §4
//! against checklist 06-e2e-matrix.md §2.2, driven directly through
//! rdb::lite::redeliver::sweep_once(shared, now_ms) with an explicit
//! "current instant" so no assertion depends on wall-clock timing --
//! idle-threshold bumps (fresh rows untouched, ordered groups head
//! only), the maxdelivery hand-off into the DLQ, acked rows never
//! redelivered, coexistence with manual XCLAIM, and group-local
//! attribution. The "off by default" anchor is process-level (see
//! lite_dlq_e2e.rs::redelivery_disabled_by_default). Kept under the
//! 400-line new-file gate (06 §1).

mod common;

use common::lite::{call, open_shared, pel_rows, text};
use rdb::conf;
use rdb::state::Shared;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

/// The engine's own wall clock (expire::now_ms) -- the same clock that
/// stamps PEL delivery instants, so "now" stays comparable to them.
fn now_ms() -> u64 {
    rdb::ds::expire::now_ms()
}

/// Drive one deterministic sweep round at "current instant" `now`.
fn sweep(shared: &Shared, now: u64) {
    let _ = rdb::lite::redeliver::sweep_once(shared, now);
}

/// A Shared whose config enables auto-redelivery at `idle_ms`
/// (lite.redelivery_idle_ms; the default Config leaves it 0 = off).
fn shared_idle(tag: &str, idle_ms: u64) -> (Shared, PathBuf) {
    let yaml = format!(
        "bind: \"127.0.0.1:{tag}\"\nstore_path: \"/tmp/\"\n\
         raft_bind_address: \"127.0.0.1:{}\"\nraft_token: \"test-token\"\n\
         lite:\n  redelivery_idle_ms: {idle_ms}\n",
        tag.parse::<u16>().unwrap() + 100
    );
    let c: conf::Config = serde_yaml::from_str(&yaml).expect("test config parses");
    let dir = std::env::temp_dir().join(format!("rdb-lite-rdly-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = rdb::store::data_path(dir.to_str().unwrap(), &c.bind);
    (open_shared(&c, &path), path)
}

/// XADD with an explicit id, asserting the echoed id reply so every
/// later PEL assertion is byte-deterministic.
fn add(shared: &Shared, stream: &[u8], id: &str) {
    assert_eq!(
        call(shared, "xadd", &[stream, id.as_bytes(), b"f", b"v"]),
        format!("${}\r\n{id}\r\n", id.len()).into_bytes(),
        "xadd {id}"
    );
}

/// XGROUP CREATE <s> <g> 0-0 MKSTREAM <extra..>; asserts +OK.
fn group(shared: &Shared, stream: &[u8], g: &[u8], extra: &[&[u8]]) {
    let mut args: Vec<&[u8]> = vec![b"create", stream, g, b"0-0", b"MKSTREAM"];
    args.extend_from_slice(extra);
    assert_eq!(
        call(shared, "xgroup", &args),
        b"+OK\r\n".to_vec(),
        "xgroup create {g:?}"
    );
}

/// One consumer's `>` delivery; asserts the highest id came through.
fn deliver(shared: &Shared, stream: &[u8], g: &[u8], c: &[u8], last: &str) {
    let r = text(&call(
        shared,
        "xreadgroup",
        &[b"group", g, c, b"streams", stream, b">"],
    ));
    assert!(r.contains(last), "delivery missing {last}: {r}");
}

/// (id, deliveries) of `XPENDING <s> <g> - + 10` range rows (row
/// tokens: [.., id, .., consumer, :idle, :times]; the >5 filter drops
/// a `*4\r\n` reply-header artifact when exactly 4 rows match).
fn rows(shared: &Shared, stream: &[u8], g: &[u8]) -> Vec<(String, u64)> {
    let reply = call(shared, "xpending", &[stream, g, b"-", b"+", b"10"]);
    pel_rows(&reply)
        .into_iter()
        .filter(|r| r.len() > 5)
        .map(|r| {
            (
                r[1].clone(),
                r[5].trim_start_matches(':').parse::<u64>().unwrap_or(0),
            )
        })
        .collect()
}

/// deliveries of one id inside `rows` (0 when absent).
fn times(rows: &[(String, u64)], id: &str) -> u64 {
    rows.iter().find(|(i, _)| i == id).map_or(0, |(_, t)| *t)
}

/// Empty XPENDING summary (the group's PEL is drained).
fn pel_empty(shared: &Shared, stream: &[u8], g: &[u8]) {
    assert_eq!(
        call(shared, "xpending", &[stream, g]),
        b"*3\r\n:0\r\n$-1\r\n$-1\r\n".to_vec(),
        "PEL drained"
    );
}

fn xrange(shared: &Shared, stream: &[u8]) -> String {
    text(&call(shared, "xrange", &[stream, b"-", b"+"]))
}

/// XLEN as reply text (":<n>\r\n"; unknown streams report 0).
fn xlen(shared: &Shared, stream: &[u8]) -> String {
    text(&call(shared, "xlen", &[stream]))
}

/// committed-id of the group in `XINFO GROUPS <s>`: the flat array is
/// positional (src/lite/info.rs groups_info), so the bulk value sits
/// two tokens after the label.
fn committed(shared: &Shared, stream: &[u8]) -> String {
    let reply = text(&call(shared, "xinfo", &[b"groups", stream]));
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let i = toks
        .iter()
        .position(|x| *x == "committed-id")
        .expect("committed-id label");
    toks[i + 2].to_string()
}

/// Only rows past the idle threshold bump: gA's rows (delivered at t0)
/// are swept at t0+idle+1 while gB's later delivery stays fresh -- and
/// each group's rows stay inside their own group/consumer (checklist
/// "重投归属": no cross-group pollution).
#[test]
fn sweep_bumps_idle_rows_only() {
    let (shared, _p) = shared_idle("45410", 400);
    let s = b"rd/q0";
    group(&shared, s, b"gA", &[]);
    group(&shared, s, b"gB", &[]);
    add(&shared, s, "1-1");
    add(&shared, s, "1-2");
    deliver(&shared, s, b"gA", b"cA", "1-2");
    let t0 = now_ms();
    // Sweep "now" = the delivery instant: nothing is idle yet.
    sweep(&shared, t0);
    assert_eq!(
        rows(&shared, s, b"gA"),
        vec![("1-1".into(), 1), ("1-2".into(), 1)],
        "fresh rows untouched"
    );
    // gB delivers >= 50ms later: still fresh when gA has gone idle
    // (the sleep only orders the two delivery instants; every timing
    // assertion goes through the explicit sweep `now`).
    sleep(Duration::from_millis(50));
    deliver(&shared, s, b"gB", b"cB", "1-2");
    sweep(&shared, t0 + 401);
    let a = rows(&shared, s, b"gA");
    assert_eq!(
        (times(&a, "1-1"), times(&a, "1-2")),
        (2, 2),
        "idle rows bumped exactly once each: {a:?}"
    );
    let b = rows(&shared, s, b"gB");
    assert_eq!(
        (times(&b, "1-1"), times(&b, "1-2")),
        (1, 1),
        "fresh rows of the other group untouched: {b:?}"
    );
}

/// A sweep that would push times past MAXDELIVERY hands the row to the
/// DLQ instead of redelivering (design two -> design one hand-off):
/// the transfer is the same atomic PEL-drain + DLQ entry + watermark
/// crossing as the claim path.
#[test]
fn sweep_respects_maxdelivery_into_dlq() {
    let (shared, _p) = shared_idle("45411", 100);
    let s = b"rd/q1";
    group(&shared, s, b"g", &[b"MAXDELIVERY", b"2"]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c1", "1-1");
    let t0 = now_ms();
    // 1 -> 2 == maxdelivery: still a redelivery, row stays in the PEL.
    sweep(&shared, t0 + 201);
    assert_eq!(rows(&shared, s, b"g"), vec![("1-1".into(), 2)]);
    // 2 -> 3 > maxdelivery: transferred, not redelivered.
    sweep(&shared, t0 + 402);
    pel_empty(&shared, s, b"g");
    let dlq = xrange(&shared, b"rd/q1/dlq");
    for needle in [
        "1-1",
        "__dlq_group",
        "__dlq_consumer",
        "__dlq_times",
        "__dlq_src",
    ] {
        assert!(dlq.contains(needle), "dlq missing {needle}: {dlq}");
    }
    assert_eq!(xlen(&shared, b"rd/q1/dlq"), ":1\r\n");
    assert_eq!(
        committed(&shared, s),
        "1-1",
        "watermark crossed the transfer"
    );
    // An empty PEL sweeps as a no-op: nothing double-transfers.
    sweep(&shared, t0 + 10_000);
    pel_empty(&shared, s, b"g");
    assert_eq!(xlen(&shared, b"rd/q1/dlq"), ":1\r\n");
}

/// Acked rows are gone from every redelivery path (checklist "不重投
/// 已 ack"): repeated sweeps find nothing, nothing lands in the DLQ,
/// and a manual claim cannot resurrect the row either.
#[test]
fn acked_rows_are_never_redelivered() {
    let (shared, _p) = shared_idle("45412", 50);
    let s = b"rd/q2";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    add(&shared, s, "1-2");
    deliver(&shared, s, b"g", b"c1", "1-2");
    assert_eq!(
        call(&shared, "xack", &[s, b"g", b"1-1", b"1-2"]),
        b":2\r\n".to_vec()
    );
    let t0 = now_ms();
    for k in 1..=3u64 {
        sweep(&shared, t0 + k * 1000);
        pel_empty(&shared, s, b"g");
    }
    assert_eq!(
        xlen(&shared, b"rd/q2/dlq"),
        ":0\r\n",
        "nothing dead-lettered"
    );
    assert_eq!(
        call(&shared, "xclaim", &[s, b"g", b"c2", b"0", b"1-1"]),
        b"*0\r\n".to_vec(),
        "manual claim of an acked id stays empty"
    );
}

/// Sweep and manual XCLAIM coexist on the same row (checklist "与手动
/// XCLAIM 并存"): after an automatic redelivery a manual claim still
/// works, the counter keeps climbing exactly one bump per action, and
/// the two paths never double-send.
#[test]
fn coexists_with_manual_claim() {
    let (shared, _p) = shared_idle("45413", 100);
    let s = b"rd/q3";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c1", "1-1");
    let t0 = now_ms();
    // Automatic redelivery: 1 -> 2.
    sweep(&shared, t0 + 201);
    assert_eq!(rows(&shared, s, b"g"), vec![("1-1".into(), 2)]);
    // Manual claim on the very same row: 2 -> 3, ownership moves.
    let cl = text(&call(&shared, "xclaim", &[s, b"g", b"c2", b"0", b"1-1"]));
    assert!(cl.contains("1-1"), "manual claim after sweep: {cl}");
    let r = rows(&shared, s, b"g");
    assert_eq!(times(&r, "1-1"), 3, "times keeps climbing: {r:?}");
    let raw = call(&shared, "xpending", &[s, b"g", b"-", b"+", b"10"]);
    let full = pel_rows(&raw)
        .into_iter()
        .filter(|x| x.len() > 5)
        .collect::<Vec<_>>();
    assert_eq!(full[0][3], "c2", "claim moved the row: {full:?}");
    // The manual claim reset the delivery clock, yet one more sweep
    // still works on the same row: 3 -> 4 (no double-send in between).
    sweep(&shared, now_ms() + 1000);
    assert_eq!(rows(&shared, s, b"g"), vec![("1-1".into(), 4)]);
    assert_eq!(
        xlen(&shared, b"rd/q3/dlq"),
        ":0\r\n",
        "no maxdelivery configured, no DLQ"
    );
}

/// Ordered groups sweep ONLY the PEL head (force_takeover path): the
/// equally-idle tail row never moves while it is not the head.
#[test]
fn ordered_group_only_head_redelivered() {
    let (shared, _p) = shared_idle("45414", 100);
    let s = b"rd/q4";
    group(&shared, s, b"g", &[b"ORDERED", b"INFLIGHT", b"2"]);
    add(&shared, s, "1-1");
    add(&shared, s, "1-2");
    deliver(&shared, s, b"g", b"c1", "1-2");
    let t0 = now_ms();
    // Both rows are equally idle, but only the head may be redelivered.
    sweep(&shared, t0 + 201);
    let r = rows(&shared, s, b"g");
    assert_eq!(
        (times(&r, "1-1"), times(&r, "1-2")),
        (2, 1),
        "head only: {r:?}"
    );
    // The head's clock was refreshed by the sweep; another round re-
    // sweeps the head and STILL leaves the tail untouched (it only
    // becomes sweepable once it IS the head).
    sweep(&shared, t0 + 402);
    let r = rows(&shared, s, b"g");
    assert_eq!(
        (times(&r, "1-1"), times(&r, "1-2")),
        (3, 1),
        "tail never swept: {r:?}"
    );
}

// ---- per-group resume cursor (in-group starvation fix) --------------------

/// Like `rows` but with a caller-chosen row cap (the shared helper
/// fixes 10, too small for the 17-row starvation ladders below).
fn rows_capped(shared: &Shared, stream: &[u8], g: &[u8], cap: usize) -> Vec<(String, u64)> {
    let cap_arg = format!("{cap}");
    let reply = call(
        shared,
        "xpending",
        &[stream, g, b"-", b"+", cap_arg.as_bytes()],
    );
    pel_rows(&reply)
        .into_iter()
        .filter(|r| r.len() > 5)
        .map(|r| {
            (
                r[1].clone(),
                r[5].trim_start_matches(':').parse::<u64>().unwrap_or(0),
            )
        })
        .collect()
}

/// Seed `n` entries `1-1 ..= 1-<n>` and deliver them all to c1 in one
/// `>` read (17 > ROW_BUDGET 16: the row past the budget is the
/// starved one).
fn seed_and_deliver(shared: &Shared, stream: &[u8], n: u64) {
    for k in 1..=n {
        add(shared, stream, &format!("1-{k}"));
    }
    deliver(shared, stream, b"g", b"c1", &format!("1-{n}"));
}

/// Refresh rows `1-1 ..= 1-16` the way a live consumer holding them
/// would (XCLAIM min-idle 0 resets the delivery clock and bumps times).
fn refresh_head_rows(shared: &Shared, stream: &[u8]) {
    for k in 1..=16u64 {
        let id = format!("1-{k}");
        let r = text(&call(
            shared,
            "xclaim",
            &[stream, b"g", b"c1", b"0", id.as_bytes()],
        ));
        assert!(r.contains(&id), "claim {id}: {r}");
    }
}

/// The starvation bug: with the first 16 pending rows perpetually
/// fresh (a live consumer keeps re-claiming them), the old always-from-
/// MIN_ID scan re-read the SAME 16 rows every round -- row 17 was
/// never examined, so it could be neither redelivered nor
/// dead-lettered. The per-group resume cursor continues STRICTLY AFTER
/// the last examined id, so the starved row comes up by round two.
#[test]
fn resume_cursor_reaches_rows_past_a_fresh_head() {
    let (shared, _p) = shared_idle("45415", 400);
    let s = b"rd/q5";
    group(&shared, s, b"g", &[]);
    seed_and_deliver(&shared, s, 17);
    // Row 17's delivery clock is now idle-old; rows 1-16 are refreshed
    // right before every round, so they are always fresh.
    sleep(Duration::from_millis(450));
    let mut resumes = rdb::lite::redeliver::ResumeMap::new();
    for round in 0..4 {
        refresh_head_rows(&shared, s);
        let (r, d, _) = rdb::lite::redeliver::sweep_from(&shared, now_ms(), b"", &mut resumes);
        assert_eq!(d, 0, "no maxdelivery configured");
        if round == 0 {
            // Budget window only: 16 fresh rows examined, nothing due,
            // the starved row still unseen.
            assert_eq!(r, 0, "fresh head rows contribute nothing");
            assert_eq!(times(&rows_capped(&shared, s, b"g", 20), "1-17"), 1);
        } else if round == 1 {
            // STRICTLY AFTER the cursor: the starved row is examined
            // (and is due) -- the bug never reached it.
            assert_eq!(r, 1, "round {round} reaches past the fresh head");
        } else {
            // Later rounds: the head is revisited after the wrap and
            // the (now fresh) starved row is left alone.
            assert_eq!(r, 0, "round {round}: nothing more is due");
        }
    }
    let tail = rows_capped(&shared, s, b"g", 20);
    assert_eq!(
        times(&tail, "1-17"),
        2,
        "starved row redelivered exactly once"
    );
    assert!(
        times(&tail, "1-16") >= 2,
        "head rows kept being refreshed: {tail:?}"
    );
}

/// The cursor lifecycle under explicit clocks: advance (full budget
/// window), strictly-after (due rows behind the cursor are NOT
/// re-examined -- progress is monotonic), wrap on a short window, head
/// revisit on the next round.
#[test]
fn resume_cursor_is_strictly_after_then_wraps() {
    let (shared, _p) = shared_idle("45416", 400);
    let s = b"rd/q6";
    group(&shared, s, b"g", &[]);
    seed_and_deliver(&shared, s, 17);
    let t0 = now_ms();
    let key = (s.to_vec(), b"g".to_vec());
    let id16 = rdb::lite::model::EntryId { ms: 1, seq: 16 };
    let mut resumes = rdb::lite::redeliver::ResumeMap::new();

    // Round 1 (all rows idle-due): budget window 1-1..1-16 examined
    // and redelivered; the cursor arms at 1-16; row 1-17 unseen.
    let (r, d, _) = rdb::lite::redeliver::sweep_from(&shared, t0 + 450, b"", &mut resumes);
    assert_eq!((r, d), (16, 0), "budget window redelivered");
    assert_eq!(
        resumes.get(&key),
        Some(&id16),
        "cursor at the last examined id"
    );
    assert_eq!(times(&rows_capped(&shared, s, b"g", 20), "1-17"), 1);

    // Round 2 (rows 1-16 are idle-AGAIN, 500ms later): strictly-after
    // means they are NOT re-examined -- only 1-17 is scanned, so the
    // tally is 1, not 17. The short window wraps the cursor.
    let (r, d, _) = rdb::lite::redeliver::sweep_from(&shared, t0 + 950, b"", &mut resumes);
    assert_eq!((r, d), (1, 0), "strictly-after: no duplicate examination");
    assert!(
        !resumes.contains_key(&key),
        "short window wraps to the head"
    );
    let mid = rows_capped(&shared, s, b"g", 20);
    assert_eq!((times(&mid, "1-1"), times(&mid, "1-17")), (2, 2), "{mid:?}");

    // Round 3 (rows 1-16 still due): the head is revisited after the
    // wrap and the cursor re-arms -- fresh head rows stay under watch.
    let (r, d, _) = rdb::lite::redeliver::sweep_from(&shared, t0 + 950, b"", &mut resumes);
    assert_eq!((r, d), (16, 0), "head revisited after the wrap");
    assert_eq!(resumes.get(&key), Some(&id16), "cursor re-armed");
}
