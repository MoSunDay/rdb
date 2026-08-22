//! Process-level Lite Mode check: kill -9 + restart on the same data dir
//! resumes group consumption from the committed watermark (at-least-once),
//! i.e. only post-watermark entries are redelivered.

mod common;

use common::lite::{cmd_full_reply, text};
use common::{cmd_one_shot, spawn_node, wait_resp_ready};

#[tokio::test]
async fn process_kill9_restart_resumes() {
    let dir = std::env::temp_dir().join(format!("rdb-lite-proc-{}", std::process::id()));
    let mut node = spawn_node(&dir, 0, true, None);
    // 30s spawn window (not 10s): on a saturated machine -- e.g. a
    // concurrent cargo build/link pinning every core -- the debug
    // binary can take >10s to bind RESP; the cluster helper uses
    // the same 30s (see common::spawn_cluster).
    wait_resp_ready(&mut node, 30).await;
    let a = node.resp.clone();
    let t = common::TOKEN;
    assert!(text(
        &cmd_one_shot(
            &a,
            t,
            &[
                b"xgroup",
                b"create",
                b"orders/q0",
                b"g",
                b"0-0",
                b"mkstream"
            ]
        )
        .await
    )
    .contains("OK"));
    assert!(
        text(&cmd_one_shot(&a, t, &[b"xadd", b"orders/q0", b"1-1", b"f", b"v"]).await)
            .contains("1-1")
    );
    assert!(
        text(&cmd_one_shot(&a, t, &[b"xadd", b"orders/q0", b"1-2", b"f", b"v"]).await)
            .contains("1-2")
    );
    assert!(text(
        &cmd_full_reply(
            &a,
            t,
            &[
                b"xreadgroup",
                b"group",
                b"g",
                b"c1",
                b"streams",
                b"orders/q0",
                b">"
            ],
            400
        )
        .await
    )
    .contains("1-2"));
    // read_one strips the trailing CRLF of single-line replies.
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xack", b"orders/q0", b"g", b"1-1"]).await),
        ":1"
    );
    // kill -9 + restart on the same data dir: only 1-2 is redelivered.
    node.kill_now();
    node.respawn();
    // 30s spawn window (not 10s): on a saturated machine -- e.g. a
    // concurrent cargo build/link pinning every core -- the debug
    // binary can take >10s to bind RESP; the cluster helper uses
    // the same 30s (see common::spawn_cluster).
    wait_resp_ready(&mut node, 30).await;
    let r = text(
        &cmd_full_reply(
            &a,
            t,
            &[
                b"xreadgroup",
                b"group",
                b"g",
                b"c1",
                b"streams",
                b"orders/q0",
                b">",
            ],
            400,
        )
        .await,
    );
    assert!(
        r.contains("1-2") && !r.contains("1-1"),
        "resume from committed: {r}"
    );
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xlen", b"orders/q0"]).await),
        ":2"
    );
}

/// kill -9 mid-flight leaves the PEL durable: after restart the two
/// un-acked ids stay pending under their old consumer, XAUTOCLAIM hands
/// them to a fresh consumer (crash-recovery redelivery), and the final
/// XACK drains the summary back to zero.
#[tokio::test]
async fn process_kill9_pel_survives_and_autoclaims() {
    let dir = std::env::temp_dir().join(format!("rdb-lite-proc-pel-{}", std::process::id()));
    let mut node = spawn_node(&dir, 1, true, None);
    // 30s spawn window (not 10s): on a saturated machine -- e.g. a
    // concurrent cargo build/link pinning every core -- the debug
    // binary can take >10s to bind RESP; the cluster helper uses
    // the same 30s (see common::spawn_cluster).
    wait_resp_ready(&mut node, 30).await;
    let a = node.resp.clone();
    let t = common::TOKEN;
    assert!(text(
        &cmd_one_shot(
            &a,
            t,
            &[
                b"xgroup",
                b"create",
                b"orders/q0",
                b"g",
                b"0-0",
                b"mkstream"
            ]
        )
        .await
    )
    .contains("OK"));
    // Explicit ids keep every later assertion stable across the restart.
    for id in [b"1-1".as_slice(), b"1-2", b"1-3"] {
        assert!(
            text(&cmd_one_shot(&a, t, &[b"xadd", b"orders/q0", id, b"f", b"v"]).await)
                .contains(std::str::from_utf8(id).unwrap())
        );
    }
    // Deliver all three to c1 (array-of-array reply -> full drain).
    let r = text(
        &cmd_full_reply(
            &a,
            t,
            &[
                b"xreadgroup",
                b"group",
                b"g",
                b"c1",
                b"count",
                b"10",
                b"streams",
                b"orders/q0",
                b">",
            ],
            400,
        )
        .await,
    );
    assert!(
        r.contains("1-1") && r.contains("1-2") && r.contains("1-3"),
        "deliver: {r}"
    );
    // c1 finishes only 1-1: 1-2/1-3 stay in its PEL.
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xack", b"orders/q0", b"g", b"1-1"]).await),
        ":1"
    );
    // SIGKILL with 1-2/1-3 pending: the committed PEL rows survive.
    node.kill_now();
    node.respawn();
    // 30s spawn window (not 10s): on a saturated machine -- e.g. a
    // concurrent cargo build/link pinning every core -- the debug
    // binary can take >10s to bind RESP; the cluster helper uses
    // the same 30s (see common::spawn_cluster).
    wait_resp_ready(&mut node, 30).await;
    // Summary = [total=2, min=1-2, max=1-3, c1, 2].
    let p = text(&cmd_full_reply(&a, t, &[b"xpending", b"orders/q0", b"g"], 400).await);
    assert!(p.starts_with("*5\r\n:2\r\n"), "pel survived kill -9: {p}");
    assert!(p.contains("1-2") && p.contains("1-3"), "pending ids: {p}");
    assert!(p.contains("c1"), "pending consumer: {p}");
    // Hand both orphans to a fresh consumer (entries section carries the
    // payload frames, so the ids show up there).
    let c = text(
        &cmd_full_reply(
            &a,
            t,
            &[
                b"xautoclaim",
                b"orders/q0",
                b"g",
                b"c2",
                b"0",
                b"0-0",
                b"count",
                b"10",
            ],
            400,
        )
        .await,
    );
    assert!(
        c.contains("1-2") && c.contains("1-3") && !c.contains("1-1"),
        "autoclaim redelivery: {c}"
    );
    // Drain: both reclaimed ids ack in one call, summary falls to [0, nil, nil].
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xack", b"orders/q0", b"g", b"1-2", b"1-3"]).await),
        ":2"
    );
    let p0 = text(&cmd_full_reply(&a, t, &[b"xpending", b"orders/q0", b"g"], 400).await);
    assert!(p0.starts_with("*3\r\n:0\r\n"), "pel drained: {p0}");
}
