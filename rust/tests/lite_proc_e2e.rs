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
    wait_resp_ready(&mut node, 10).await;
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
    wait_resp_ready(&mut node, 10).await;
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
