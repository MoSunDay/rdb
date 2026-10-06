//! Process-level wiring e2e of the background idle-redelivery sweep: the
//! REAL spawned binary with `lite.redelivery_idle_ms` armed (the in-round
//! semantics live in lite_redeliver_e2e.rs; the off-by-default anchor in
//! lite_dlq_e2e.rs::redelivery_disabled_by_default). Proves the spawned
//! loop end-to-end: an abandoned PEL row's delivery count visibly rises
//! through XPENDING with no client anywhere near it, while a row below
//! the idle threshold is never touched during the same window.

mod common;

use common::lite::{boot_yaml, pending_rows, resp_full, resp_text};
use std::time::{Duration, Instant};

/// Sweep-arm idle threshold, ms (engine tick is 200ms, so a due row is
/// re-handed within ~idle+200ms; the poll budget below has ample margin).
const IDLE_MS: u64 = 800;
/// Poll budget for the positive check (a due row must bump by then even
/// on a heavily loaded machine: debug binary + 200ms rhythm).
const RISE_BUDGET: Duration = Duration::from_secs(15);
/// Observation window for the negative check: comfortably below IDLE_MS,
/// so a healthy sweep -- tick 200ms -- cannot legitimately touch a row
/// delivered at the window's start (margin = IDLE_MS - window - tick).
const SUB_IDLE_WINDOW: Duration = Duration::from_millis(200);

/// Times-delivered of one pending id (0 when the PEL row is gone).
async fn times_of(a: &str, s: &[u8], id: &str) -> u64 {
    pending_rows(a, s, b"g", 10)
        .await
        .into_iter()
        .find(|(i, _, _)| i == id)
        .map(|(_, _, n)| n)
        .unwrap_or(0)
}

/// XADD + `>` read by a consumer that then goes silent (no ack, no
/// claim, no further reads: the row sits in its PEL forever).
async fn abandon(a: &str, s: &[u8], id: &str) {
    // resp_text strips the trailing CRLF of single-line replies.
    assert!(
        resp_text(a, &[b"xadd", s, id.as_bytes(), b"f", b"v"])
            .await
            .contains(id),
        "xadd {id}"
    );
    let r = resp_full(
        a,
        &[b"xreadgroup", b"group", b"g", b"c1", b"streams", s, b">"],
    )
    .await;
    assert!(r.contains(id), "consumer c1 reads {id}: {r}");
}

/// The spawned sweep loop redelivers an abandoned PEL row to its CURRENT
/// consumer: XPENDING's delivery count visibly rises without any client
/// action, and it keeps rising across rounds (claim primitive, every due
/// round), while a freshly delivered row below the idle threshold keeps
/// its count of 1 throughout a sub-idle observation window.
#[tokio::test]
async fn spawned_sweep_raises_delivery_times_of_idle_rows() {
    let yaml = format!("lite:\n  redelivery_idle_ms: {IDLE_MS}\n");
    let (_node, a) = boot_yaml("sweep", &yaml).await;
    assert_eq!(
        resp_text(
            &a,
            &[b"xgroup", b"create", b"t/q0", b"g", b"0-0", b"MKSTREAM"],
        )
        .await,
        "+OK"
    );
    abandon(&a, b"t/q0", "1-1").await;
    assert_eq!(times_of(&a, b"t/q0", "1-1").await, 1, "delivery #1");
    // Positive check: poll until the background loop (no client around)
    // has re-handed the row at least twice -- one bump could hide behind
    // a claim race, two prove a rhythm, not an accident.
    let deadline = Instant::now() + RISE_BUDGET;
    loop {
        let times = times_of(&a, b"t/q0", "1-1").await;
        assert!(times > 0, "row left the PEL: nothing should ack it");
        if times >= 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "spawned sweep never raised times past 3 within {RISE_BUDGET:?}; \
             times={}, node context follows\n{}",
            times_of(&a, b"t/q0", "1-1").await,
            _node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // The sweep re-hands to the CURRENT consumer: ownership never moved.
    let owners: Vec<String> = pending_rows(&a, b"t/q0", b"g", 10)
        .await
        .into_iter()
        .map(|(_, c, _)| c)
        .collect();
    assert_eq!(owners, vec!["c1".to_string()], "redelivery keeps the owner");
    // Negative check with margin: a row delivered NOW sits below the
    // idle threshold for the whole window -- the sweep must not touch it
    // (times stays 1), unlike the abandoned sibling rising beside it.
    abandon(&a, b"t/q0", "1-2").await;
    tokio::time::sleep(SUB_IDLE_WINDOW).await;
    let fresh = times_of(&a, b"t/q0", "1-2").await;
    assert_eq!(
        fresh, 1,
        "sub-idle row touched after {SUB_IDLE_WINDOW:?} (< idle {IDLE_MS}ms)"
    );
    // And the abandoned row is still being swept in the background.
    assert!(
        times_of(&a, b"t/q0", "1-1").await >= 3,
        "idle row keeps rising while the fresh one does not"
    );
}
