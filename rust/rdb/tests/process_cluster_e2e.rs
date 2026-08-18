//! Scenario A -- process-level port of /tmp/drill.py: launch the REAL rdb
//! binary x3 with REAL yaml configs (tempdir-backed), join them into a
//! cluster via the raft HTTP API, and walk the full drill checklist against
//! live RESP sockets.

mod common;

use common::{
    all_ctx, auth_reply, cluster_init, cmd_one_shot, contains_bytes, raw_exchange, spawn_node,
    wait_cluster_nodes_list_all, wait_leader, wait_resp_ready, TOKEN,
};

/// The bytes of one `-MOVED <slot> <addr>` reply as `(slot, addr)`.
fn moved_parts(reply: &[u8]) -> Vec<&[u8]> {
    reply.split(|&c| c == b' ').skip(1).collect()
}

/// node0 bootstraps and leads, then 1..=2 join via node0's raft-http addr;
/// the start_cluster phases spread out so the PRE-init cluster gate (step 4)
/// stays observable between leader-wait and `CLUSTER INIT`.
async fn bring_up_cluster(dir: &std::path::Path) -> (Vec<common::ProcNode>, usize) {
    let mut nodes = Vec::new();
    let mut first = spawn_node(dir, 0, true, None);
    wait_resp_ready(&mut first, 30).await;
    nodes.push(first);
    let l0 = wait_leader(&nodes, 60).await;
    assert_eq!(
        l0,
        0,
        "bootstrapped node0 must lead before joins\n{}",
        all_ctx(&nodes)
    );

    let join = nodes[0].http.clone();
    for id in 1..3 {
        let mut node = spawn_node(dir, id, false, Some(&join));
        wait_resp_ready(&mut node, 30).await;
        nodes.push(node);
    }
    let leader = wait_leader(&nodes, 60).await;
    (nodes, leader)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drill_py_scenario_again_three_real_processes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (nodes, leader) = bring_up_cluster(dir.path()).await;
    let leader_resp = nodes[leader].resp.clone();

    // 2. pre-AUTH probe on a raw conn: exactly the fork's NOAUTH error.
    let probe = raw_exchange(&leader_resp, b"*2\r\n$3\r\nget\r\n$1\r\nx\r\n").await;
    assert_eq!(
        probe,
        b"-ERR: NOAUTH",
        "pre-auth `get x` must be refused\n{}",
        nodes[leader].ctx()
    );

    // 3. explicit AUTH with the correct token replies +OK
    //    (every later command re-auths implicitly through cmd_one_shot).
    let auth = auth_reply(&leader_resp, TOKEN).await;
    assert_eq!(
        auth,
        b"+OK",
        "AUTH with the right token\n{}",
        nodes[leader].ctx()
    );

    // 4. before init: `cluster nodes` is refused with the Go typo text.
    let refused = cmd_one_shot(&leader_resp, TOKEN, &[b"cluster", b"nodes"]).await;
    assert!(
        contains_bytes(&refused, b"cluster not ready") && contains_bytes(&refused, b"instanes01"),
        "pre-init cluster nodes must carry the Go error text\nreply={refused:?}\n{}",
        nodes[leader].ctx()
    );

    // 5. init on the leader, then every node lists every bind.
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 30).await;
    for n in &nodes {
        let r = cmd_one_shot(&n.resp, TOKEN, &[b"cluster", b"nodes"]).await;
        for b in &binds {
            assert!(
                contains_bytes(&r, b.as_bytes()),
                "cluster nodes on {} must list {b}\nreply={r:?}\n{}",
                n.resp,
                n.ctx()
            );
        }
    }

    // 6. slot routing: exactly one owner per key, everyone else -MOVED.
    let mut owner: Option<usize> = None;
    for (i, n) in nodes.iter().enumerate() {
        let r = cmd_one_shot(&n.resp, TOKEN, &[b"set", b"drillkey1", b"val1"]).await;
        if r == b"+OK" {
            assert!(
                owner.is_none(),
                "two +OK owners for drillkey1: {owner:?} and {i}"
            );
            owner = Some(i);
        } else if r.starts_with(b"-MOVED ") {
            let parts = moved_parts(&r);
            assert_eq!(
                parts.len(),
                2,
                "MOVED must be `-MOVED <slot> <addr>`: {r:?}"
            );
            assert!(
                !parts[0].is_empty() && parts[0].iter().all(u8::is_ascii_digit),
                "MOVED slot must be decimal: {r:?}"
            );
            assert!(
                binds.iter().any(|b| b.as_bytes() == parts[1]),
                "MOVED addr must be one of the cluster binds: {r:?}"
            );
        } else {
            panic!(
                "set drillkey1 on {} answered neither +OK nor -MOVED\nreply={r:?}\n{}",
                n.resp,
                n.ctx()
            );
        }
    }
    let owner = match owner {
        Some(o) => o,
        None => panic!(
            "no node answered +OK for set drillkey1\n{}",
            all_ctx(&nodes)
        ),
    };
    assert_eq!(
        cmd_one_shot(&nodes[owner].resp, TOKEN, &[b"get", b"drillkey1"]).await,
        b"$4\r\nval1",
        "get on the owner\n{}",
        nodes[owner].ctx()
    );
    for (i, n) in nodes.iter().enumerate() {
        if i != owner {
            let r = cmd_one_shot(&n.resp, TOKEN, &[b"get", b"drillkey1"]).await;
            assert!(
                r.starts_with(b"-MOVED "),
                "get on non-owner {} must MOVED\nreply={r:?}\n{}",
                n.resp,
                n.ctx()
            );
        }
    }
    assert_eq!(
        cmd_one_shot(&nodes[owner].resp, TOKEN, &[b"del", b"drillkey1"]).await,
        b":1",
        "del on the owner\n{}",
        nodes[owner].ctx()
    );
    assert_eq!(
        cmd_one_shot(&nodes[owner].resp, TOKEN, &[b"get", b"drillkey1"]).await,
        b"$-1",
        "get after del is nil\n{}",
        nodes[owner].ctx()
    );

    // 7. hash tags: {tag7}a and {tag7}b share one owner (same slot).
    let mut tag_owners: Vec<usize> = Vec::new();
    for k in [b"{tag7}a".as_slice(), b"{tag7}b".as_slice()] {
        for (i, n) in nodes.iter().enumerate() {
            let r = cmd_one_shot(&n.resp, TOKEN, &[b"set", k, b"x"]).await;
            if r == b"+OK" {
                tag_owners.push(i);
                break;
            }
            assert!(
                r.starts_with(b"-MOVED "),
                "set {k:?} on {} answered neither +OK nor -MOVED\nreply={r:?}\n{}",
                n.resp,
                n.ctx()
            );
        }
    }
    assert_eq!(
        tag_owners.len(),
        2,
        "each tagged key needs an owner\n{}",
        all_ctx(&nodes)
    );
    assert_eq!(
        tag_owners[0],
        tag_owners[1],
        "{{tag7}}a and {{tag7}}b must land on the same owner node\n{}",
        all_ctx(&nodes)
    );

    // 8. control plane: writes are leader-only, reads are cluster-wide.
    let follower = (0..nodes.len())
        .find(|i| *i != leader)
        .expect("a follower exists");
    let r = cmd_one_shot(
        &nodes[follower].resp,
        TOKEN,
        &[b"raft", b"set", b"rk1", b"rv1"],
    )
    .await;
    assert!(
        contains_bytes(&r, b"internal error err: not leader"),
        "raft set on follower must fail with the Go error text\nreply={r:?}\n{}",
        nodes[follower].ctx()
    );
    assert_eq!(
        cmd_one_shot(
            &nodes[leader].resp,
            TOKEN,
            &[b"raft", b"set", b"rk1", b"rv1"]
        )
        .await,
        b"+OK",
        "raft set on the leader\n{}",
        nodes[leader].ctx()
    );
    for n in &nodes {
        let r = cmd_one_shot(&n.resp, TOKEN, &[b"raft", b"get", b"rk1"]).await;
        assert_eq!(
            r,
            b"$3\r\nrv1",
            "raft get rk1 must see the replicated value (bulk frame) on {}\n{}",
            n.resp,
            n.ctx()
        );
    }

    // 9. unknown command, verbatim Go text.
    assert_eq!(
        cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"bogusxyz"]).await,
        b"-ERR unknown command 'bogusxyz'",
        "unknown command text\n{}",
        nodes[leader].ctx()
    );
}
