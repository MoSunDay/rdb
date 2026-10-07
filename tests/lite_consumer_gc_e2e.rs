//! Lite idle consumer GC e2e (in-process, deterministic clock): the
//! triple criteria of plans/2026-10-06-mq-gap pool #14 against
//! checklist 06-e2e-matrix.md, driven directly through
//! rdb::lite::consumer_gc::gc_once(shared, now_ms) with an explicit
//! "current instant" so no assertion depends on wall-clock timing --
//! an idle member with an EMPTY PEL is collected (XINFO CONSUMERS and
//! XINFO FULL shrink), a PEL holder / a parked XREADGROUP reader / a
//! live ordered-queue owner all survive past the threshold, an ack
//! refreshes the activity clock, the knob-off default collects
//! nothing, and the redeliver sweep keeps rotating correctly through
//! interleaved GC rounds. The spawned-task wiring and kill -9
//! durability are process-level (lite_consumer_gc_proc_e2e.rs).

mod common;

use common::lite::{call, open_shared, pel_rows, pending_rows4, text};
use rdb::conf;
use rdb::state::Shared;
use std::path::PathBuf;

/// The engine's own wall clock (expire::now_ms) -- the same clock that
/// stamps registry seen/delivery instants.
fn now_ms() -> u64 {
    rdb::ds::expire::now_ms()
}

/// Drive one deterministic GC round at "current instant" `now`.
fn gc(shared: &Shared, now: u64) -> usize {
    rdb::lite::consumer_gc::gc_once(shared, now)
}

/// A Shared (Arc-wrapped: the parked-reader test hands it to a thread)
/// whose config arms the idle consumer GC at `gc_ms` and (optionally)
/// the auto-redelivery sweep at `redelivery_ms`.
fn shared_gc(tag: &str, gc_ms: u64, redelivery_ms: u64) -> (std::sync::Arc<Shared>, PathBuf) {
    let yaml = format!(
        "bind: \"127.0.0.1:{tag}\"\nstore_path: \"/tmp/\"\n\
         raft_bind_address: \"127.0.0.1:{}\"\nraft_token: \"test-token\"\n\
         lite:\n  consumer_gc_ms: {gc_ms}\n  redelivery_idle_ms: {redelivery_ms}\n",
        tag.parse::<u16>().unwrap() + 100
    );
    let c: conf::Config = serde_yaml::from_str(&yaml).expect("test config parses");
    let dir = std::env::temp_dir().join(format!("rdb-lite-cgc-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = rdb::store::data_path(dir.to_str().unwrap(), &c.bind);
    (std::sync::Arc::new(open_shared(&c, &path)), path)
}

/// XADD with an explicit id, asserting the echoed id reply.
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

/// XGROUP CREATECONSUMER; asserts :1 (newly created).
fn create_consumer(shared: &Shared, stream: &[u8], g: &[u8], c: &[u8]) {
    assert_eq!(
        call(shared, "xgroup", &[b"createconsumer", stream, g, c]),
        b":1\r\n".to_vec(),
        "createconsumer {c:?}"
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

/// XACK one id; asserts :1.
fn ack(shared: &Shared, stream: &[u8], g: &[u8], id: &str) {
    assert_eq!(
        call(shared, "xack", &[stream, g, id.as_bytes()]),
        b":1\r\n".to_vec(),
        "xack {id}"
    );
}

/// Consumer names of `XINFO CONSUMERS <s> <g>`, sorted (positional
/// tokens: the bulk value sits two tokens after each `name` label).
fn consumers(shared: &Shared, stream: &[u8], g: &[u8]) -> Vec<String> {
    let reply = text(&call(shared, "xinfo", &[b"consumers", stream, g]));
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let mut names: Vec<String> = toks
        .windows(3)
        .filter(|w| w[0] == "name")
        .map(|w| w[2].to_string())
        .collect();
    names.sort();
    names
}

/// Threshold under test (ms): "keep" checks run at +400 (safely below),
/// "collect" checks at +600 (safely above; stamps settle in setup).
const GC_MS: u64 = 500;

/// The triple criteria in one round: an empty-PEL member past the
/// threshold is collected (both XINFO CONSUMERS and XINFO FULL shrink),
/// a PEL holder survives the very same round, and a member one margin
/// below the threshold is still recent.
#[test]
fn idle_empty_pel_member_collected_pel_holder_kept() {
    let (shared, _p) = shared_gc("45520", GC_MS, 0);
    let s = b"gc/q0";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c-idle", "1-1");
    ack(&shared, s, b"g", "1-1"); // c-idle: empty PEL, clock = ack time
    add(&shared, s, "1-2");
    deliver(&shared, s, b"g", b"c-hold", "1-2"); // c-hold: keeps 1-2
    create_consumer(&shared, s, b"g", b"c-never"); // never delivered
    let t0 = now_ms();
    // One margin below the threshold: nothing is collectable yet.
    assert_eq!(gc(&shared, t0 + 400), 0, "sub-threshold round collects");
    assert_eq!(
        consumers(&shared, s, b"g"),
        vec![
            "c-hold".to_string(),
            "c-idle".to_string(),
            "c-never".to_string()
        ],
        "all members listed before the threshold"
    );
    // Past the threshold: the two empty-PEL members go, the holder
    // stays -- same round, same clock.
    assert_eq!(gc(&shared, t0 + 600), 2, "idle members collected");
    assert_eq!(consumers(&shared, s, b"g"), vec!["c-hold".to_string()]);
    let full = text(&call(&shared, "xinfo", &[b"stream", s, b"full"]));
    assert!(full.contains("c-hold"), "FULL keeps the holder: {full}");
    assert!(!full.contains("c-idle"), "FULL dropped the idle: {full}");
    assert!(
        !full.contains("c-never"),
        "FULL dropped the never-run: {full}"
    );
    // The holder's PEL is untouched by the collection.
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(rows.len(), 1, "one pending row: {rows:?}");
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3),
        ("1-2", "c-hold", 1),
        "pending row intact"
    );
}

/// An ack refreshes the seen clock: a consumer whose delivery is far
/// past the threshold but whose ACK is recent survives, and is only
/// collected once the ack itself ages past the threshold.
#[test]
fn ack_refreshes_seen_clock() {
    let (shared, _p) = shared_gc("45521", GC_MS, 0);
    let s = b"gc/q0";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c1", "1-1");
    // Let the DELIVERY age past the threshold, then ack: without the
    // ack-side refresh this round would collect (delivery is 700ms old).
    std::thread::sleep(std::time::Duration::from_millis(700));
    ack(&shared, s, b"g", "1-1");
    let t_ack = now_ms();
    assert_eq!(gc(&shared, t_ack + 400), 0, "recent ack keeps the member");
    assert_eq!(consumers(&shared, s, b"g"), vec!["c1".to_string()]);
    // The ack itself now ages past the threshold: collected.
    assert_eq!(gc(&shared, t_ack + 600), 1, "post-ack idle collected");
    assert_eq!(consumers(&shared, s, b"g"), Vec::<String>::new());
}

/// A consumer blocked in a waiting XREADGROUP is leased and never
/// collected; once its BLOCK expires (no park left, no PEL, idle), the
/// same clock collects it.
#[test]
fn parked_xreadgroup_reader_survives() {
    let (shared, _p) = shared_gc("45522", GC_MS, 0);
    let s = b"gc/q0";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c-park", "1-1");
    ack(&shared, s, b"g", "1-1"); // empty PEL: only the park protects
    let t0 = now_ms();
    // A second thread blocks 300ms in XREADGROUP (call() parks the
    // calling thread server-side); the park lease must veto the GC.
    let (sh, stream) = (std::sync::Arc::clone(&shared), s.to_vec());
    let reader = std::thread::spawn(move || {
        let _ = call(
            &sh,
            "xreadgroup",
            &[
                b"group", b"g", b"c-park", b"block", b"300", b"streams", &stream, b">",
            ],
        );
    });
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(gc(&shared, t0 + 600), 0, "parked reader is never collected");
    assert_eq!(consumers(&shared, s, b"g"), vec!["c-park".to_string()]);
    reader.join().expect("reader thread");
    // Park gone (BLOCK expired), PEL empty, idle past threshold: goes.
    assert_eq!(gc(&shared, t0 + 601), 1, "unparked idle reader collected");
    assert_eq!(consumers(&shared, s, b"g"), Vec::<String>::new());
}

/// Ordered-group owners are covered by the ownership lease: a live
/// owner with an EMPTY PEL survives past the GC threshold, a bystander
/// of the same group does not, and once the ownership lease itself
/// expires the abandoned owner goes too (lease, not immortality).
#[test]
fn ordered_owner_leased_bystander_collected() {
    let (shared, _p) = shared_gc("45523", GC_MS, 0);
    let s = b"gc/q0";
    group(&shared, s, b"g", &[b"ORDERED"]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c-owner", "1-1"); // takes the queue
    ack(&shared, s, b"g", "1-1"); // empty PEL, still the live owner
    create_consumer(&shared, s, b"g", b"c-by");
    let t0 = now_ms();
    assert_eq!(gc(&shared, t0 + 600), 1, "only the bystander collected");
    assert_eq!(consumers(&shared, s, b"g"), vec!["c-owner".to_string()]);
    // Past the 30s default ownership lease with no refresh: the
    // abandoned owner is collectable like anyone else.
    assert_eq!(gc(&shared, t0 + 31_000), 1, "expired-lease owner collected");
    assert_eq!(consumers(&shared, s, b"g"), Vec::<String>::new());
}

/// Knob off (the 0 default): the round itself is a no-op -- nothing is
/// ever collected through gc_once on a default config.
#[test]
fn knob_off_collects_nothing() {
    let (shared, _p) = common::lite::shared_at("45524");
    let s = b"gc/q0";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c1", "1-1");
    ack(&shared, s, b"g", "1-1");
    create_consumer(&shared, s, b"g", b"c2");
    assert_eq!(
        gc(&shared, now_ms() + 10_000),
        0,
        "disabled GC collects nothing"
    );
    assert_eq!(
        consumers(&shared, s, b"g"),
        vec!["c1".to_string(), "c2".to_string()],
        "registry intact with the knob off"
    );
}

/// GC interleaved with the redeliver sweep: the sweep keeps rotating
/// (exactly one re-hand per due row per round, owner unchanged) while
/// GC rounds run between them, and the PEL-empty member is collected
/// without disturbing the sweep's view of the group.
#[test]
fn gc_interleaves_with_redeliver_sweep() {
    let (shared, _p) = shared_gc("45525", GC_MS, GC_MS);
    let s = b"gc/q0";
    group(&shared, s, b"g", &[]);
    add(&shared, s, "1-1");
    deliver(&shared, s, b"g", b"c1", "1-1"); // abandoned row
    create_consumer(&shared, s, b"g", b"c2"); // idle member
    let t0 = now_ms();
    // Round 1: the sweep re-hands the due row to its owner (times 2),
    // the GC collects only the PEL-empty member.
    let _ = rdb::lite::redeliver::sweep_once(&shared, t0 + GC_MS + 1);
    assert_eq!(gc(&shared, t0 + GC_MS + 1), 1, "idle member collected");
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3),
        ("1-1", "c1", 2),
        "swept exactly once, owner unchanged: {rows:?}"
    );
    assert_eq!(consumers(&shared, s, b"g"), vec!["c1".to_string()]);
    // Round 2 (GC first, sweep after): the sweep's clocks still advance
    // by exactly one per round -- no double delivery, no lost row.
    assert_eq!(gc(&shared, t0 + 2 * GC_MS + 2), 0, "nothing left to GC");
    let _ = rdb::lite::redeliver::sweep_once(&shared, t0 + 2 * GC_MS + 2);
    let rows = pending_rows4(&shared, s, b"g", 10);
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3),
        ("1-1", "c1", 3),
        "second sweep bumped exactly once more: {rows:?}"
    );
    // Ack drains the PEL; the next GC round then reclaims c1 too, and
    // a further sweep round is a clean no-op.
    ack(&shared, s, b"g", "1-1");
    let t_ack = now_ms();
    let _ = rdb::lite::redeliver::sweep_once(&shared, t_ack + GC_MS + 1);
    assert_eq!(
        gc(&shared, t_ack + GC_MS + 1),
        1,
        "drained member collected"
    );
    assert_eq!(consumers(&shared, s, b"g"), Vec::<String>::new());
    assert!(pel_rows(&call(&shared, "xpending", &[s, b"g"])).is_empty());
}
