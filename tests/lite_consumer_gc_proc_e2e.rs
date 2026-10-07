//! Process-level wiring e2e of the background idle consumer GC: the
//! REAL spawned binary with `lite.consumer_gc_ms` armed (the in-round
//! semantics live in lite_consumer_gc_e2e.rs; the off-by-default
//! anchor lives here too -- a default-config node collects nothing).
//! Proves the spawned loop end-to-end: idle members vanish from XINFO
//! CONSUMERS / XINFO FULL with no client anywhere near them, active
//! members (PEL holder, parked XREADGROUP reader, ordered-queue owner)
//! survive the same window, a collected member does NOT reappear after
//! kill -9 + restart (the removal batch is synced), and the collected
//! name transparently re-registers on its next delivery.

mod common;

use common::lite::{boot, boot_yaml, park_read, pending_rows, resp_full, resp_text};
use std::time::{Duration, Instant};

/// GC idle threshold armed in the node's yaml (the spawned task ticks
/// every 1s, so a due member is collected within ~threshold+1s+margin;
/// the poll budget below has ample room on a loaded machine).
const GC_MS: u64 = 700;
/// Poll budget for the positive checks (debug binary + 1s rhythm).
const POLL: Duration = Duration::from_secs(15);
/// Window proving the negative checks: comfortably past GC_MS, so a
/// running GC would have collected an idle member by its end.
const IDLE_WINDOW: Duration = Duration::from_millis(2500);

fn yaml() -> String {
    format!("lite:\n  consumer_gc_ms: {GC_MS}\n")
}

/// Consumer names of `XINFO CONSUMERS <s> <g>`, sorted (positional
/// tokens: the bulk value sits two tokens after each `name` label).
async fn consumers(a: &str, s: &[u8], g: &[u8]) -> Vec<String> {
    let reply = resp_full(a, &[b"xinfo", b"consumers", s, g]).await;
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let mut names: Vec<String> = toks
        .windows(3)
        .filter(|w| w[0] == "name")
        .map(|w| w[2].to_string())
        .collect();
    names.sort();
    names
}

/// Poll `XINFO CONSUMERS` until the roster equals `want` (context-
/// tagged panic on budget end; the empty roster is `&[]`).
async fn until_consumers(a: &str, s: &[u8], g: &[u8], want: &[&str], what: &str) {
    let want: Vec<String> = want.iter().map(|x| x.to_string()).collect();
    let start = Instant::now();
    loop {
        let got = consumers(a, s, g).await;
        if got == want {
            return;
        }
        assert!(
            start.elapsed() < POLL,
            "{what}: stuck at {got:?}, want {want:?} (budget {POLL:?})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Auto-id XADD reply -> the bare id (the bulk reply is two lines:
/// the `$<len>` header and the payload; the payload is the id).
async fn add_auto(a: &str, s: &[u8], v: &[u8]) -> String {
    let reply = resp_text(a, &[b"xadd", s, b"*", b"f", v]).await;
    let id = reply.trim().lines().last().unwrap_or("").trim().to_string();
    assert!(!id.is_empty() && id.contains('-'), "xadd reply: {reply}");
    id
}

/// XADD (id *) + `>` delivery to `c` + XACK: the member ends with an
/// EMPTY PEL and a fresh activity stamp, then goes silent.
async fn drain(a: &str, s: &[u8], g: &[u8], c: &[u8]) {
    let id = add_auto(a, s, b"v").await;
    let read = resp_full(a, &[b"xreadgroup", b"group", g, c, b"streams", s, b">"]).await;
    assert!(read.contains("v"), "delivery: {read}");
    let acked = resp_text(a, &[b"xack", s, g, id.as_bytes()]).await;
    assert_eq!(acked.trim(), ":1", "ack {id}: {acked}");
}

/// XADD (id *) + `>` delivery to `c`, NO ack: the member keeps its PEL.
async fn abandon(a: &str, s: &[u8], g: &[u8], c: &[u8]) -> String {
    let id = add_auto(a, s, b"v").await;
    let read = resp_full(a, &[b"xreadgroup", b"group", g, c, b"streams", s, b">"]).await;
    assert!(read.contains("v"), "delivery: {read}");
    id
}

/// The spawned GC collects idle members with no client driving it, and
/// the XINFO FULL roster shrinks with XINFO CONSUMERS.
#[tokio::test]
async fn spawned_gc_collects_idle_members() {
    let (_node, a) = boot_yaml("44251", &yaml()).await;
    assert_eq!(
        resp_text(
            &a,
            &[b"xgroup", b"create", b"t/q0", b"g", b"0-0", b"MKSTREAM"]
        )
        .await,
        "+OK"
    );
    drain(&a, b"t/q0", b"g", b"c1").await; // acked: idle from now on
    assert_eq!(
        resp_text(&a, &[b"xgroup", b"createconsumer", b"t/q0", b"g", b"c2"]).await,
        ":1"
    );
    assert_eq!(consumers(&a, b"t/q0", b"g").await.len(), 2, "both listed");
    until_consumers(&a, b"t/q0", b"g", &[], "spawned GC collects idle members").await;
    // The FULL view shrinks the same way: no consumer roster left.
    let full = resp_full(&a, &[b"xinfo", b"stream", b"t/q0", b"full"]).await;
    assert!(!full.contains("c1") && !full.contains("c2"), "FULL: {full}");
}

/// Active members survive the very window that collects an idle one:
/// a PEL holder, a reader parked in XREADGROUP BLOCK 0 and a live
/// ordered-queue owner (empty PEL, ownership lease) all stay listed.
#[tokio::test]
async fn active_members_survive_while_idle_vanishes() {
    let (_node, a) = boot_yaml("44252", &yaml()).await;
    assert_eq!(
        resp_text(
            &a,
            &[b"xgroup", b"create", b"t/q0", b"g", b"0-0", b"MKSTREAM"]
        )
        .await,
        "+OK"
    );
    let _held = abandon(&a, b"t/q0", b"g", b"c-hold").await; // PEL holder
    drain(&a, b"t/q0", b"g", b"c-park").await; // empty PEL, then parks
    let _parked = park_read(
        &a,
        &[
            b"xreadgroup",
            b"group",
            b"g",
            b"c-park",
            b"block",
            b"0",
            b"streams",
            b"t/q0",
            b">",
        ],
    )
    .await;
    assert_eq!(
        resp_text(
            &a,
            &[
                b"xgroup",
                b"create",
                b"t/q1",
                b"g2",
                b"0-0",
                b"MKSTREAM",
                b"ORDERED"
            ],
        )
        .await,
        "+OK"
    );
    drain(&a, b"t/q1", b"g2", b"c-ord").await; // ordered owner, empty PEL
    assert_eq!(
        resp_text(
            &a,
            &[b"xgroup", b"createconsumer", b"t/q0", b"g", b"c-idle"]
        )
        .await,
        ":1"
    );
    until_consumers(
        &a,
        b"t/q0",
        b"g",
        &["c-hold", "c-park"],
        "idle member collected, active kept",
    )
    .await;
    // Stability, not just a blip: past the threshold + margin the
    // roster is unchanged (the ordered owner rides its 30s lease).
    tokio::time::sleep(IDLE_WINDOW).await;
    assert_eq!(
        consumers(&a, b"t/q0", b"g").await,
        vec!["c-hold".to_string(), "c-park".to_string()],
        "active members kept"
    );
    assert_eq!(
        consumers(&a, b"t/q1", b"g2").await,
        vec!["c-ord".to_string()],
        "ordered owner kept"
    );
    let owners: Vec<String> = pending_rows(&a, b"t/q0", b"g", 10)
        .await
        .into_iter()
        .map(|(_, c, _)| c)
        .collect();
    assert_eq!(owners, vec!["c-hold".to_string()], "PEL intact");
}

/// kill -9 durability of the removal: a collected member does NOT
/// reappear after SIGKILL + restart (the GC batch is a synced write),
/// the surviving member and its PEL come through whole, and the
/// collected name transparently re-registers on its next delivery.
#[tokio::test]
async fn kill9_collected_members_stay_gone() {
    let (mut node, a) = boot_yaml("44253", &yaml()).await;
    assert_eq!(
        resp_text(
            &a,
            &[b"xgroup", b"create", b"t/q0", b"g", b"0-0", b"MKSTREAM"]
        )
        .await,
        "+OK"
    );
    drain(&a, b"t/q0", b"g", b"c-gone").await; // will be collected
    let held = abandon(&a, b"t/q0", b"g", b"c-keep").await; // survives
    until_consumers(&a, b"t/q0", b"g", &["c-keep"], "pre-crash collection").await;
    // kill -9 (SIGKILL: no graceful flush anywhere) + respawn on the
    // same data dir (same yaml: the GC task re-arms itself).
    node.kill_now();
    node.respawn();
    common::wait_resp_ready(&mut node, 30).await;
    let a2 = node.resp.clone();
    assert_eq!(
        consumers(&a2, b"t/q0", b"g").await,
        vec!["c-keep".to_string()],
        "collected member stayed gone, survivor intact"
    );
    let rows = pending_rows(&a2, b"t/q0", b"g", 10).await;
    assert_eq!(rows.len(), 1, "one pending row: {rows:?}");
    assert_eq!(rows[0].1, "c-keep", "row owner: {rows:?}");
    // The collected name is not poisoned: a later delivery by the same
    // name re-registers it (first-sight rewrite, one batch).
    let id = add_auto(&a2, b"t/q0", b"v2").await;
    let read = resp_full(
        &a2,
        &[
            b"xreadgroup",
            b"group",
            b"g",
            b"c-gone",
            b"streams",
            b"t/q0",
            b">",
        ],
    )
    .await;
    assert!(read.contains("v2"), "re-delivery to c-gone: {read}");
    assert_eq!(
        resp_text(&a2, &[b"xack", b"t/q0", b"g", id.as_bytes()])
            .await
            .trim(),
        ":1"
    );
    assert_eq!(
        consumers(&a2, b"t/q0", b"g").await,
        vec!["c-gone".to_string(), "c-keep".to_string()],
        "re-registered after first sight"
    );
    // At-least-once across the crash: the restart rewound the group's
    // delivered watermark to the committed one, so that `>` read ALSO
    // re-delivered the survivor's unacked row and re-owned it as
    // c-gone's -- c-keep now holds an empty PEL with a pre-crash seen
    // stamp, so the GC (correctly) reclaims it while c-gone, now the
    // PEL holder, must survive.
    until_consumers(
        &a2,
        b"t/q0",
        b"g",
        &["c-gone"],
        "rewind re-attribution + GC",
    )
    .await;
    // And the GC is still armed after the restart: drain c-gone's PEL
    // (the re-owned row) and the freshly idle name goes again.
    assert_eq!(
        resp_text(&a2, &[b"xack", b"t/q0", b"g", held.as_bytes()])
            .await
            .trim(),
        ":1",
        "ack the re-owned row"
    );
    until_consumers(&a2, b"t/q0", b"g", &[], "post-restart GC re-collects").await;
}

/// Knob off (the 0 default): a default-config node collects nothing --
/// an idle member with an empty PEL stays listed indefinitely.
#[tokio::test]
async fn knob_off_default_collects_nothing() {
    let (_node, a) = boot("44254").await;
    assert_eq!(
        resp_text(
            &a,
            &[b"xgroup", b"create", b"t/q0", b"g", b"0-0", b"MKSTREAM"]
        )
        .await,
        "+OK"
    );
    drain(&a, b"t/q0", b"g", b"c1").await;
    assert_eq!(
        resp_text(&a, &[b"xgroup", b"createconsumer", b"t/q0", b"g", b"c2"]).await,
        ":1"
    );
    tokio::time::sleep(Duration::from_millis(2000)).await;
    assert_eq!(
        consumers(&a, b"t/q0", b"g").await,
        vec!["c1".to_string(), "c2".to_string()],
        "default config never collects"
    );
}
