//! Scenario: `migrate task <slot> <src> <dst>` moves one slot end to end
//! on the REAL 3-process cluster: MIGRATING/IMPORTING gating, key
//! transport (raw strings + a hash) via the DUMP/RESTORE hop, NODE/STABLE
//! ownership handover, raft-replicated owner-map convergence on the
//! bystander node, and the JSON task record in `migrate list`.

mod common;

use common::lite::cmd_full_reply;
use common::{all_ctx, cmd_one_shot, contains_bytes, start_cluster, TOKEN};

/// Poll `probe` until it returns true or `secs` elapse.
async fn wait_until<F, Fut>(nodes: &[common::ProcNode], secs: u64, mut probe: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if probe().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    panic!("condition not met within {secs}s\n{}", all_ctx(nodes));
}

/// `CLUSTER KEYSLOT` -> u16.
async fn keyslot(nodes: &[common::ProcNode], leader: usize, key: &[u8]) -> u16 {
    let r = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"cluster", b"keyslot", key]).await;
    assert!(
        r.starts_with(b":"),
        "keyslot reply {r:?}\n{}",
        all_ctx(nodes)
    );
    // `cmd_one_shot` strips the trailing CRLF, so the int is r[1..].
    String::from_utf8_lossy(&r[1..])
        .parse()
        .unwrap_or_else(|_| panic!("keyslot not numeric: {r:?}"))
}

/// Equal-split owner index for `slot` on a 3-node cluster.
fn band_owner(slot: u16) -> usize {
    if slot <= 5461 {
        0
    } else if slot <= 10922 {
        1
    } else {
        2
    }
}

/// Retry a write until it returns `want` (a fresh cluster settles over a
/// few seconds: topology ticker rounds, raft catch-up). Panics with the
/// last reply when the budget is exhausted.
async fn retry_reply(
    nodes: &[common::ProcNode],
    idx: usize,
    args: &[&[u8]],
    want: &[u8],
    secs: u64,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut last: Vec<u8>;
    loop {
        let r = cmd_one_shot(&nodes[idx].resp, TOKEN, args).await;
        if r == want {
            return;
        }
        last = r;
        assert!(
            std::time::Instant::now() < deadline,
            "retry_reply {args:?} on n{idx} never returned {want:?}; last={last:?}\n{}",
            all_ctx(nodes)
        );
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

/// The payload of a `$N\r\n<payload>` bulk reply from `cmd_one_shot`
/// (which returns the frame with the trailing CRLF stripped).
fn bulk_payload(reply: &[u8]) -> &[u8] {
    assert_eq!(reply.first(), Some(&b'$'), "not a bulk reply: {reply:?}");
    let line_end = reply.iter().position(|&b| b == b'\r').expect("bulk crlf");
    let n: usize = String::from_utf8_lossy(&reply[1..line_end])
        .parse()
        .unwrap();
    &reply[line_end + 2..][..n]
}

/// The bulk keys of a `CLUSTER GETKEYSINSLOT` array reply.
fn array_keys(reply: &[u8]) -> Vec<Vec<u8>> {
    let mut keys = Vec::new();
    let mut rest = reply; // full frame, trailing CRLF included
    let hdr = rest
        .iter()
        .position(|&b| b == b'\r')
        .expect("array header crlf");
    rest = &rest[hdr + 2..]; // consume `*N\r\n`
    while !rest.is_empty() {
        assert_eq!(rest[0], b'$', "unexpected reply {reply:?}");
        let line_end = rest.iter().position(|&b| b == b'\r').expect("crlf");
        let len: usize = String::from_utf8_lossy(&rest[1..line_end]).parse().unwrap();
        rest = &rest[line_end + 2..];
        keys.push(rest[..len].to_vec());
        rest = &rest[len + 2..];
    }
    keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_task_moves_slot_between_real_nodes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (nodes, leader) = start_cluster(dir.path(), 3).await;

    // A hash-tagged key family: every key lands in one slot.
    let slot = keyslot(&nodes, leader, b"{mig}s1").await;
    let src = band_owner(slot);
    let dst = (src + 1) % 3;
    let bystander = (src + 2) % 3;
    eprintln!(
        "slot {slot} src=n{} dst=n{} bystander=n{}",
        src, dst, bystander
    );

    // Seed strings + a hash on the source owner (retrying through the
    // post-init settle window).
    for (k, v) in [(b"{mig}s1", b"v1"), (b"{mig}s2", b"v2")] {
        retry_reply(&nodes, src, &[b"set", k, v], b"+OK", 60).await;
    }
    retry_reply(
        &nodes,
        src,
        &[b"hset", b"{mig}h", b"f1", b"hv1", b"f2", b"hv2"],
        b":2",
        60,
    )
    .await;
    let r = cmd_full_reply(
        &nodes[src].resp,
        TOKEN,
        &[
            b"cluster",
            b"getkeysinslot",
            slot.to_string().as_bytes(),
            b"100",
        ],
        400,
    )
    .await;
    let seeded = array_keys(&r);
    assert_eq!(
        seeded.len(),
        3,
        "seeded keys {seeded:?}\n{}",
        all_ctx(&nodes)
    );

    // One-command orchestration on the leader (which may be any node).
    let r = cmd_one_shot(
        &nodes[leader].resp,
        TOKEN,
        &[
            b"migrate",
            b"task",
            slot.to_string().as_bytes(),
            nodes[src].resp.as_bytes(),
            nodes[dst].resp.as_bytes(),
        ],
    )
    .await;
    assert_eq!(r, b"+OK", "migrate task\n{}", all_ctx(&nodes));

    // The JSON task record reports done with the moved count.
    let r = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"migrate", b"list"]).await;
    assert!(
        contains_bytes(&r, b"\"status\":\"done\"") && contains_bytes(&r, b"\"moved\":3"),
        "task record {r:?}\n{}",
        all_ctx(&nodes)
    );

    // Data is served by the destination...
    wait_until(&nodes, 30, || async {
        let r = cmd_one_shot(&nodes[dst].resp, TOKEN, &[b"get", b"{mig}s1"]).await;
        r.first() == Some(&b'$') && bulk_payload(&r) == b"v1"
    })
    .await;
    let r = cmd_one_shot(&nodes[dst].resp, TOKEN, &[b"get", b"{mig}s2"]).await;
    assert_eq!(
        bulk_payload(&r),
        b"v2",
        "dst serves s2\n{}",
        all_ctx(&nodes)
    );
    let r = cmd_full_reply(&nodes[dst].resp, TOKEN, &[b"hgetall", b"{mig}h"], 400).await;
    assert!(
        contains_bytes(&r, b"hv1") && contains_bytes(&r, b"hv2"),
        "hash moved intact {r:?}\n{}",
        all_ctx(&nodes)
    );

    // ...the source no longer serves it (MOVED toward the destination),
    // and the bystander converges via the raft owner map (3s ticker).
    wait_until(&nodes, 30, || async {
        let r = cmd_one_shot(&nodes[src].resp, TOKEN, &[b"get", b"{mig}s1"]).await;
        r.starts_with(b"-MOVED ") && contains_bytes(&r, nodes[dst].resp.as_bytes())
    })
    .await;
    wait_until(&nodes, 30, || async {
        let r = cmd_one_shot(&nodes[bystander].resp, TOKEN, &[b"get", b"{mig}s1"]).await;
        r.starts_with(b"-MOVED ") && contains_bytes(&r, nodes[dst].resp.as_bytes())
    })
    .await;

    // GETKEYSINSLOT flipped: empty on the source, the 3 keys on the dst.
    wait_until(&nodes, 30, || async {
        let r = cmd_one_shot(
            &nodes[src].resp,
            TOKEN,
            &[
                b"cluster",
                b"getkeysinslot",
                slot.to_string().as_bytes(),
                b"100",
            ],
        )
        .await;
        r == b"*0"
    })
    .await;
    let r = cmd_full_reply(
        &nodes[dst].resp,
        TOKEN,
        &[
            b"cluster",
            b"getkeysinslot",
            slot.to_string().as_bytes(),
            b"100",
        ],
        400,
    )
    .await;
    let moved_keys = array_keys(&r);
    assert_eq!(
        moved_keys.len(),
        3,
        "dst keys {moved_keys:?}\n{}",
        all_ctx(&nodes)
    );

    // A second task for the same slot is refused while one runs: the
    // busy guard only matters in-flight, so after completion the same
    // slot can migrate again (src <- dst round trip).
    let r = cmd_one_shot(
        &nodes[leader].resp,
        TOKEN,
        &[
            b"migrate",
            b"task",
            slot.to_string().as_bytes(),
            nodes[dst].resp.as_bytes(),
            nodes[src].resp.as_bytes(),
        ],
    )
    .await;
    assert_eq!(r, b"+OK", "reverse migration\n{}", all_ctx(&nodes));
    wait_until(&nodes, 30, || async {
        let r = cmd_one_shot(&nodes[src].resp, TOKEN, &[b"get", b"{mig}s1"]).await;
        r.first() == Some(&b'$') && bulk_payload(&r) == b"v1"
    })
    .await;
}
