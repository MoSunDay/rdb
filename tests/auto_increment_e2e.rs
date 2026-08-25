//! AUTO_INCREMENT end-to-end over a real rdb process: DDL validation
//! (one integer auto column, must be the pk), id allocation on INSERT
//! (NULL / 0 / missing column), the explicit-value counter bump,
//! LAST_INSERT_ID() and its SET form, and counter persistence across a
//! node restart (the counter is raft-replicated catalog state).

mod common;

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";

async fn connect(node: &common::ProcNode) -> mysql_async::Conn {
    let port = node
        .mysql
        .rsplit(':')
        .next()
        .expect("mysql port")
        .parse::<u16>()
        .expect("mysql port digits");
    let opts = || {
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .user(Some("root"))
            .pass(Some(PASS))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => return c,
            Err(mysql_async::Error::Io(_)) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

/// DDL and counter-bumping INSERTs are leader-only writes; the bootstrap
/// node becomes leader within a second or two, so retry until it sticks.
async fn run(conn: &mut mysql_async::Conn, sql: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if std::time::Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                panic!("run {sql}: {e}")
            }
        }
    }
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect()
}

/// Text-protocol cells are length-prefixed bytes, ints included.
fn int(i: i64) -> MVal {
    MVal::Bytes(i.to_string().into_bytes())
}

fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}

async fn world(name: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-ai-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut conn = connect(&node).await;
    run(
        &mut conn,
        "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    (node, conn)
}

#[tokio::test]
async fn auto_increment_full_flow() {
    let (_node, mut c) = world("flow").await;

    // ---- DDL validation (MySQL 1075/1063) ----
    assert!(
        c.query_drop(
            "CREATE TABLE bad (id BIGINT AUTO_INCREMENT PRIMARY KEY, \
                      seq BIGINT AUTO_INCREMENT)"
        )
        .await
        .is_err(),
        "two auto columns rejected"
    );
    assert!(
        c.query_drop(
            "CREATE TABLE bad (id BIGINT PRIMARY KEY, \
                      seq BIGINT AUTO_INCREMENT)"
        )
        .await
        .is_err(),
        "auto column outside the pk rejected"
    );
    assert!(
        c.query_drop("CREATE TABLE bad (id VARCHAR(10) AUTO_INCREMENT PRIMARY KEY)")
            .await
            .is_err(),
        "non-integer auto column rejected"
    );

    // ---- allocation: NULL / 0 / omitted column, one statement ----
    run(
        &mut c,
        "INSERT INTO ai (id, v) VALUES (NULL, 'a'), (0, 'b'), (NULL, 'c')",
    )
    .await;
    let mut got = rows(&mut c, "SELECT id, v FROM ai ORDER BY id").await;
    got.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(
        got,
        vec![
            vec![int(1), s("a")],
            vec![int(2), s("b")],
            vec![int(3), s("c")]
        ],
        "consecutive ids within one multi-row INSERT"
    );

    // ---- LAST_INSERT_ID(): first generated id of the last INSERT ----
    assert_eq!(
        rows(&mut c, "SELECT LAST_INSERT_ID() FROM ai LIMIT 1").await,
        vec![vec![int(1)]]
    );

    // ---- explicit value >= counter bumps it to value+1 ----
    run(&mut c, "INSERT INTO ai (id, v) VALUES (100, 'explicit')").await;
    // (explicit rows do NOT touch LAST_INSERT_ID)
    assert_eq!(
        rows(&mut c, "SELECT LAST_INSERT_ID() FROM ai LIMIT 1").await,
        vec![vec![int(1)]]
    );
    run(&mut c, "INSERT INTO ai (v) VALUES ('after')").await;
    assert_eq!(
        rows(&mut c, "SELECT id FROM ai WHERE v = 'after'").await,
        vec![vec![int(101)]],
        "next auto id after explicit 100 is 101"
    );
    assert_eq!(
        rows(&mut c, "SELECT LAST_INSERT_ID() FROM ai LIMIT 1").await,
        vec![vec![int(101)]]
    );

    // ---- LAST_INSERT_ID(n): sets and returns n ----
    assert_eq!(
        rows(&mut c, "SELECT LAST_INSERT_ID(7) FROM ai LIMIT 1").await,
        vec![vec![int(7)]]
    );
    assert_eq!(
        rows(&mut c, "SELECT LAST_INSERT_ID() FROM ai LIMIT 1").await,
        vec![vec![int(7)]]
    );
}

/// The next-value counter is raft-replicated state: a restarted node
/// resumes from the persisted counter instead of restarting at 1.
#[tokio::test]
async fn counter_survives_restart() {
    let (mut node, mut c) = world("restart").await;
    run(&mut c, "INSERT INTO ai (v) VALUES ('a'), ('b')").await;
    let before = rows(&mut c, "SELECT id FROM ai ORDER BY id").await;
    assert_eq!(before, vec![vec![int(1)], vec![int(2)]]);

    node.respawn();
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;
    run(&mut c, "INSERT INTO ai (v) VALUES ('post-restart')").await;
    let after = rows(&mut c, "SELECT id FROM ai WHERE v = 'post-restart'").await;
    assert_eq!(
        after,
        vec![vec![int(65)]],
        "resumes from the persisted batch reservation (1 + 64)"
    );
}

/// 3-process cluster: the counter is raft-replicated catalog state, so
/// ids allocated on the leader are visible and UNIQUE everywhere (each
/// node's scatter-gather SELECT returns the same full set), statements
/// larger than one reservation batch stay consecutive inside the
/// statement, and non-leader nodes refuse to allocate (the same
/// leader-only rule DDL already follows).
#[tokio::test]
async fn cluster_allocates_unique_ids_through_raft() {
    use common::{
        cluster_init, cmd_one_shot, spawn_node_sql, wait_cluster_nodes_list_all, wait_leader,
        ProcNode, TOKEN,
    };

    let dir = std::env::temp_dir().join(format!("rdb-sql-ai-cluster-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut nodes: Vec<ProcNode> = Vec::new();
    let mut first = spawn_node_sql(&dir, 0, true, None);
    wait_resp_ready(&mut first, 30).await;
    wait_mysql_ready(&first, 15).await;
    nodes.push(first);
    assert_eq!(wait_leader(&nodes, 60).await, 0, "node0 must lead first");
    let join = nodes[0].http.clone();
    for id in 1..3 {
        let mut node = spawn_node_sql(&dir, id, false, Some(&join));
        wait_resp_ready(&mut node, 30).await;
        wait_mysql_ready(&node, 15).await;
        nodes.push(node);
    }
    let leader = wait_leader(&nodes, 60).await;
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 30).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let reg = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"raft", b"get", b"sql_nodes"]).await;
        let ready = binds
            .iter()
            .all(|b| common::contains_bytes(&reg, b.as_bytes()))
            && !common::contains_bytes(&reg, b"\"sql_rpc\":\"\"");
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "registry never converged: {reg:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let mut lc = connect(&nodes[leader]).await;
    run(
        &mut lc,
        "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    // 70 rows > one 64-id reservation batch: still consecutive within
    // the statement (one raft round-trip covers the whole walk); the
    // 70-row statement persists next=71, so 'tail' takes 71.
    let vals: Vec<String> = (0..70).map(|i| format!("('r{i}')")).collect();
    run(
        &mut lc,
        &format!("INSERT INTO ai (v) VALUES {}", vals.join(", ")),
    )
    .await;
    run(&mut lc, "INSERT INTO ai (v) VALUES ('tail')").await;

    // A spanning explicit-id write from the leader: every slot owner
    // participates in the 2PC and advances its oracle to this txn's
    // ts, so the checks below are not subject to the engine's
    // documented idle-node read staleness (`global_hi` may lag; the
    // dist-read e2e relies on the same trick).
    let span: Vec<String> = (0..40).map(|i| format!("({}, 's{i}')", 1000 + i)).collect();
    run(
        &mut lc,
        &format!("INSERT INTO ai (id, v) VALUES {}", span.join(", ")),
    )
    .await;

    let expected: Vec<i64> = (1..=71).chain(1000..=1039).collect();
    // Gather from every node: ids are unique by construction (each pk
    // arrives from exactly its slot owner), which is the assertion.
    for (i, node) in nodes.iter().enumerate() {
        let mut c = connect(node).await;
        let mut got: Vec<i64> = rows(&mut c, "SELECT id FROM ai")
            .await
            .into_iter()
            .map(|r| match &r[0] {
                MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
                v => panic!("node {i}: non-bytes id {v:?}"),
            })
            .collect();
        got.sort_unstable();
        got.dedup();
        assert_eq!(got, expected, "node {i} sees the unique id set");
    }

    // Non-leaders refuse to allocate: same leader-only rule as DDL.
    let follower = (leader + 1) % nodes.len();
    let mut fc = connect(&nodes[follower]).await;
    let err = fc
        .query_drop("INSERT INTO ai (v) VALUES ('nope')")
        .await
        .expect_err("allocation must be leader-only");
    assert!(
        err.to_string().contains("leader"),
        "follower error should mention the leader: {err}"
    );
}
