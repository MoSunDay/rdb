//! Scenario A -- process-level port of /tmp/drill.py: launch the REAL rdb
//! binary x3 with REAL yaml configs (tempdir-backed), join them into a
//! cluster via the raft HTTP API, and walk the full drill checklist against
//! live RESP sockets.

mod common;

use common::lite::cmd_full_reply;
use common::{
    all_ctx, auth_reply, cluster_init, cmd_one_shot, contains_bytes, raw_exchange, spawn_node,
    start_cluster, wait_cluster_nodes_list_all, wait_leader, wait_resp_ready, TOKEN,
};

/// One raw HTTP/1.1 GET against a node's raft control API (/join,
/// /depart): write the request head, read to EOF (the server closes the
/// connection after the response).
async fn http_get(addr: &str, target: &str) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = match tokio::net::TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(e) => return format!("<CONN-ERR {e}>").into_bytes(),
    };
    let req = format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if sock.write_all(req.as_bytes()).await.is_err() {
        return b"<WRITE-ERR>".to_vec();
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(35), sock.read(&mut chunk)).await
        {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    buf
}

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

/// D7b regression: the join decision must follow PERSISTED RAFT STATE,
/// not data-dir existence. A first join against a dead join address dies
/// fatally (Go parity), but by then the process has already created the
/// raft data dir with its RocksDB CURRENT marker. With the old
/// dir-existence decision the retry silently skipped joining and the
/// node stayed a lone uninitialized follower forever; with
/// `Raft::is_initialized()` it joins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_first_join_is_retried_on_restart() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Lone leader first: the joiner needs a live cluster to retry into.
    let mut nodes = vec![spawn_node(dir.path(), 0, true, None)];
    wait_resp_ready(&mut nodes[0], 30).await;
    assert_eq!(
        wait_leader(&nodes, 60).await,
        0,
        "bootstrapped node0 must lead\n{}",
        all_ctx(&nodes)
    );

    // First attempt: RAFT_JOIN_ADDR points at a dead port (instant
    // refusal). The join fails and the process exits(1) -- AFTER
    // new_raft_node already created the raft dir + RocksDB CURRENT.
    let mut joiner = spawn_node(dir.path(), 1, false, Some("127.0.0.1:1"));
    let status = joiner.child.wait().expect("joiner must exit");
    assert_eq!(
        status.code(),
        Some(1),
        "failed join is fatal (Go parity)\n{}",
        joiner.ctx()
    );
    let raft_dir = joiner.dir.join(&joiner.resp).join("raft");
    assert!(
        raft_dir.join("CURRENT").is_file(),
        "precondition: the failed attempt already left a created raft dir with a CURRENT marker\n{}",
        joiner.ctx()
    );

    // Retry: SAME config path + data dir, now against the LIVE leader.
    // (common::spawn_child is private, so the respawn is spelled out.)
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&joiner.stderr_path)
        .expect("open stderr log");
    joiner.child = std::process::Command::new(env!("CARGO_BIN_EXE_rdb"))
        .arg("-config")
        .arg(&joiner.config_path)
        .env_remove("RAFT_BOOTSTRAP")
        .env_remove("RDB_BEACON")
        .env_remove("RDB_DEBUG_REPL")
        .env_remove("RUST_LOG")
        .env("RAFT_JOIN_ADDR", &nodes[0].http)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr))
        .spawn()
        .expect("respawn joiner");

    // RESP binds LAST: readiness proves the join RPC succeeded this time.
    wait_resp_ready(&mut joiner, 30).await;
    nodes.push(joiner);

    // And the joiner must be a real member: init from the leader lists
    // every bind on EVERY node; a node that skipped joining never sees
    // the instances value and stays "cluster not ready".
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[0], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 30).await;
}
/// Membership scenario: /depart a LIVE non-leader through the leader's
/// control API, watch the raft configuration drop it (quorum keeps
/// serving), then /join the still-running node back and watch full
/// membership -- and replication to it -- return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depart_live_follower_then_rejoin_restores_membership() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().expect("tempdir");
    let (nodes, leader) = start_cluster(dir.path(), 3).await;
    let follower = (0..nodes.len())
        .find(|i| *i != leader)
        .expect("a follower exists");
    let other = (0..nodes.len())
        .find(|i| *i != leader && *i != follower)
        .expect("a second follower exists");

    // /depart the follower by its raft-tcp address on the leader's API:
    // 200 + body "ok" (the Go handler quirk: errors also answer 200, so
    // the body text is what proves success).
    let target = format!(
        "/depart?peerAddress={}&raft-token={}",
        nodes[follower].raft, TOKEN
    );
    let resp = http_get(&nodes[leader].http, &target).await;
    assert!(
        contains_bytes(&resp, b"HTTP/1.1 200 OK") && contains_bytes(&resp, b"\r\n\r\nok"),
        "depart must answer 200 + ok: {:?}\n{}",
        String::from_utf8_lossy(&resp),
        nodes[leader].ctx()
    );

    // The leader's latest_configuration drops the departed voter (raft
    // metrics sync into `raft nodes` at most every 500ms).
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"raft", b"nodes"]).await;
        if !contains_bytes(&r, nodes[follower].raft.as_bytes()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "departed voter still configured\nlast={r:?}\n{}",
            nodes[leader].ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Quorum (2 of 2 remaining voters) still serves control-plane writes,
    // and the surviving follower sees them replicated.
    assert_eq!(
        cmd_one_shot(
            &nodes[leader].resp,
            TOKEN,
            &[b"raft", b"set", b"depkey", b"depval"]
        )
        .await,
        b"+OK",
        "raft set with the departed voter gone\n{}",
        nodes[leader].ctx()
    );
    let want = b"$6\r\ndepval".to_vec();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = cmd_one_shot(&nodes[other].resp, TOKEN, &[b"raft", b"get", b"depkey"]).await;
        if r == want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "survivor never saw depkey\nlast={r:?}\n{}",
            nodes[other].ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // Re-join the STILL-RUNNING departed node: /join re-adds it as a
    // learner (blocking until caught up) and back into the voters.
    let target = format!(
        "/join?peerAddress={}&raft-token={}",
        nodes[follower].raft, TOKEN
    );
    let resp = http_get(&nodes[leader].http, &target).await;
    assert!(
        contains_bytes(&resp, b"HTTP/1.1 200 OK") && contains_bytes(&resp, b"\r\n\r\nok"),
        "re-join must answer 200 + ok: {:?}\n{}",
        String::from_utf8_lossy(&resp),
        nodes[leader].ctx()
    );

    // Full membership again: every raft address back in the leader's
    // configuration.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"raft", b"nodes"]).await;
        let all = nodes.iter().all(|n| contains_bytes(&r, n.raft.as_bytes()));
        if all {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rejoined voter not back in the configuration\nlast={r:?}\n{}",
            nodes[leader].ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // The rejoined voter receives new writes again.
    assert_eq!(
        cmd_one_shot(
            &nodes[leader].resp,
            TOKEN,
            &[b"raft", b"set", b"rejoinkey", b"rejoinval"]
        )
        .await,
        b"+OK",
        "raft set after rejoin\n{}",
        nodes[leader].ctx()
    );
    let want = b"$9\r\nrejoinval".to_vec();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = cmd_one_shot(
            &nodes[follower].resp,
            TOKEN,
            &[b"raft", b"get", b"rejoinkey"],
        )
        .await;
        if r == want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rejoined follower never saw rejoinkey\nlast={r:?}\n{}",
            nodes[follower].ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// MIGRATE task/list over a REAL raft: a leader-written task replicates
/// to the FSM key `migrate_task` and lists back with underscores turned
/// into spaces on every node; error paths keep the Go quirk of an
/// ERROR-reply usage message, and a follower's task apply fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_task_list_over_real_raft_and_error_paths() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().expect("tempdir");
    let (nodes, leader) = start_cluster(dir.path(), 3).await;
    let follower = (0..nodes.len())
        .find(|i| *i != leader)
        .expect("a follower exists");

    // No task yet: Go strings.Split quirk -- one EMPTY item, not zero.
    assert_eq!(
        cmd_full_reply(&nodes[leader].resp, TOKEN, &[b"migrate", b"list"], 400).await,
        b"*1\r\n$0\r\n\r\n".to_vec(),
        "empty task lists as one empty item\n{}",
        nodes[leader].ctx()
    );

    // Usage / arity / unknown subcommand: the helper text is an ERROR.
    for args in [
        vec![b"migrate".as_slice()],
        vec![b"migrate", b"task", b"a", b"b"],
        vec![b"migrate", b"bogus", b"x"],
        vec![b"migrate", b"help"],
    ] {
        let r = cmd_one_shot(&nodes[leader].resp, TOKEN, &args).await;
        assert_eq!(
            r,
            b"-migrate [ list | task ]".to_vec(),
            "{args:?}\n{}",
            nodes[leader].ctx()
        );
    }

    // A follower cannot apply: hashicorp's "not leader" surfaces as the
    // Go "Raft Apply failed" error.
    assert_eq!(
        cmd_one_shot(
            &nodes[follower].resp,
            TOKEN,
            &[b"migrate", b"task", b"no", b"no", b"no"]
        )
        .await,
        b"-Raft Apply failed".to_vec(),
        "migrate task on a follower\n{}",
        nodes[follower].ctx()
    );

    // Leader write: MIGRATE task src dst count -> +OK, replicated.
    assert_eq!(
        cmd_one_shot(
            &nodes[leader].resp,
            TOKEN,
            &[b"migrate", b"task", b"alpha", b"beta", b"gamma"]
        )
        .await,
        b"+OK",
        "migrate task on the leader\n{}",
        nodes[leader].ctx()
    );
    let want = b"*1\r\n$16\r\nalpha beta gamma\r\n".to_vec();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let r = cmd_full_reply(&nodes[follower].resp, TOKEN, &[b"migrate", b"list"], 400).await;
        if r == want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "follower never listed the replicated task\nlast={r:?}\n{}",
            nodes[follower].ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // The leader lists the same task (underscores -> spaces).
    assert_eq!(
        cmd_full_reply(&nodes[leader].resp, TOKEN, &[b"migrate", b"list"], 400).await,
        want,
        "leader lists the task\n{}",
        nodes[leader].ctx()
    );
}
