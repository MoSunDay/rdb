//! E1 -- SIGTERM/SIGINT graceful shutdown against the REAL rdb binary: on
//! either signal the process must log, flush the Lite group-offset
//! watermarks IMMEDIATELY (not waiting for the 200ms ticker) and exit 0.
//! After a restart the stream entries survive and `XREADGROUP >` resumes
//! from the committed watermark, so only the un-acked entry is redelivered.

mod common;

use std::time::{Duration, Instant};

use common::lite::{cmd_full_reply, text};
use common::{
    auth_reply, cluster_init, cmd_one_shot, spawn_node, wait_cluster_nodes_list_all, wait_leader,
    wait_resp_ready, ProcNode, TOKEN,
};

/// The harness `kill_now` is SIGKILL, useless for graceful-shutdown
/// coverage: send a REAL signal via the shell `kill`, then wait (bounded,
/// 10s) for the child to exit on its own and return its exit code. Panics
/// with the node's stderr tail if the process ignores the signal.
async fn signal_and_wait_exit(node: &mut ProcNode, sig: &str) -> i32 {
    let pid = node.child.id();
    let kill = std::process::Command::new("kill")
        .arg(format!("-{sig}"))
        .arg(pid.to_string())
        .status()
        .expect("spawn kill");
    assert!(kill.success(), "kill -{sig} {pid} failed: {kill}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(st) = node.child.try_wait().expect("try_wait child") {
            return st.code().unwrap_or(-1);
        }
        assert!(
            Instant::now() < deadline,
            "rdb still alive 10s after SIG{sig}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll plain `GET <key>` until it returns the bulk frame for `want`
/// (absorbs the post-restart pre-topology-sync window; 3s ticker).
async fn poll_get_bulk(node: &ProcNode, key: &str, want: &str, secs: u64) {
    let want_reply = format!("${}\r\n{want}", want.len()).into_bytes();
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let r = cmd_one_shot(&node.resp, TOKEN, &[b"get", key.as_bytes()]).await;
        if r == want_reply {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "get {key} never returned {want}\nlast reply={r:?}\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// SIGTERM: exit code 0 (not death-by-signal) with the graceful log line,
/// AND the Lite offset state lands before the exit -- after respawn the 3
/// XADD entries survive and a fresh `XREADGROUP >` delivers ONLY the
/// un-acked third entry (resume from the committed watermark).
///
/// Coverage note: XACK persists the committed watermark synchronously
/// (src/lite/ack.rs), and `offset::load` clamps delivered back to committed
/// on restart, so the redelivery assertion holds even for SIGKILL -- it
/// proves the graceful path preserves the at-least-once contract, while
/// exit-0 + the log line are what actually discriminate E1 (a binary
/// without signal handling dies with a signal status and logs nothing).
/// The shutdown flush still drains the delivered-watermark dirty set
/// immediately; that is not observable post-restart today. No sleep before
/// SIGTERM by design.
#[tokio::test]
async fn sigterm_exits_zero_and_flushes_lite_offsets() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    let a = node.resp.clone();
    let t = TOKEN;
    assert_eq!(text(&auth_reply(&a, t).await), "+OK");

    // 3 entries, then a group at 0-0.
    for id in ["1-1", "1-2", "1-3"] {
        assert!(
            text(&cmd_one_shot(&a, t, &[b"xadd", b"orders/q0", id.as_bytes(), b"f", b"v"]).await)
                .contains(id),
            "xadd {id}\n{}",
            node.ctx()
        );
    }
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xgroup", b"create", b"orders/q0", b"g", b"0-0"]).await),
        "+OK",
        "xgroup create\n{}",
        node.ctx()
    );

    // Deliver all 3 to c1 (advances the memory-only delivered watermark)...
    let delivered = text(
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
        delivered.contains("1-1") && delivered.contains("1-2") && delivered.contains("1-3"),
        "c1 should get all 3: {delivered}\n{}",
        node.ctx()
    );
    // ...then commit the first 2 (advances the persisted committed watermark).
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xack", b"orders/q0", b"g", b"1-1", b"1-2"]).await),
        ":2",
        "xack 1-1 1-2\n{}",
        node.ctx()
    );

    // IMMEDIATELY SIGTERM (no sleep): exit code must be 0, graceful log line
    // must be on stderr, and the flush must have landed before the exit.
    let code = signal_and_wait_exit(&mut node, "TERM").await;
    assert_eq!(code, 0, "SIGTERM must exit 0\n{}", node.ctx());
    assert!(
        node.stderr_tail()
            .contains("received SIGTERM, shutting down gracefully"),
        "graceful-shutdown log line missing\n{}",
        node.ctx()
    );

    // Restart on the same data dir (persistent raft state, no join needed).
    node.respawn();
    wait_resp_ready(&mut node, 30).await;

    // All 3 entries survived.
    assert_eq!(
        text(&cmd_one_shot(&a, t, &[b"xlen", b"orders/q0"]).await),
        ":3",
        "xlen after restart\n{}",
        node.ctx()
    );
    let range = text(&cmd_full_reply(&a, t, &[b"xrange", b"orders/q0", b"-", b"+"], 400).await);
    assert!(
        range.contains("1-1") && range.contains("1-2") && range.contains("1-3"),
        "xrange after restart lost entries: {range}\n{}",
        node.ctx()
    );

    // Fresh `>` delivery: ONLY 1-3 (resumes from committed = 1-2).
    let redelivered = text(
        &cmd_full_reply(
            &a,
            t,
            &[
                b"xreadgroup",
                b"group",
                b"g",
                b"c2",
                b"streams",
                b"orders/q0",
                b">",
            ],
            400,
        )
        .await,
    );
    assert!(
        redelivered.contains("1-3") && !redelivered.contains("1-1") && !redelivered.contains("1-2"),
        "restart must redeliver only the un-acked 1-3: {redelivered}\n{}",
        node.ctx()
    );
}

/// Cheap SIGINT variant: same graceful path (log + flush + exit 0), plain
/// SET/GET proves the data plane survived the restart.
#[tokio::test]
async fn sigint_also_exits_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Single-node cluster (SET needs topology: bootstrap, lead, init).
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    let l = wait_leader(std::slice::from_ref(&node), 60).await;
    assert_eq!(l, 0, "the lone bootstrapped node must lead\n{}", node.ctx());
    let binds = vec![node.resp.clone()];
    cluster_init(&node, &binds).await;
    wait_cluster_nodes_list_all(std::slice::from_ref(&node), &binds, 30).await;

    assert_eq!(
        cmd_one_shot(&node.resp, TOKEN, &[b"set", b"sigkey", b"sigvalue"]).await,
        b"+OK",
        "set sigkey\n{}",
        node.ctx()
    );

    let code = signal_and_wait_exit(&mut node, "INT").await;
    assert_eq!(code, 0, "SIGINT must exit 0\n{}", node.ctx());
    assert!(
        node.stderr_tail()
            .contains("received SIGINT, shutting down gracefully"),
        "graceful-shutdown log line missing\n{}",
        node.ctx()
    );

    node.respawn();
    wait_resp_ready(&mut node, 60).await;
    poll_get_bulk(&node, "sigkey", "sigvalue", 30).await;
}
