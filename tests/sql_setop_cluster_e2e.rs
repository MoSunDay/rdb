//! Phase-1 cluster e2e: compound queries (UNION / CTE) over cluster
//! reads. Columnar tables fan out to every member and row tables
//! scatter-gather per slot band, so a UNION or CTE issued on ANY node
//! must materialize each operand cluster-wide before composing --
//! exactly the P0 gather discipline applied to set operations.
//! Seeding follows sql_join_cluster_e2e.rs: distinct columnar ranges
//! per node, one 2PC row insert, and an oracle-view lift insert.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{
    cluster_init, cmd_one_shot, spawn_node_sql, wait_cluster_nodes_list_all, wait_leader,
    wait_mysql_ready, wait_resp_ready, ProcNode, TOKEN,
};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";

async fn connect(node: &ProcNode) -> mysql_async::Conn {
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
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => return c,
            Err(mysql_async::Error::Io(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

async fn ddl(conn: &mut mysql_async::Conn, sql: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                panic!("ddl {sql}: {e}")
            }
        }
    }
}

async fn poll_catalog(conn: &mut mysql_async::Conn, table: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let rs: Vec<mysql_async::Row> = conn.query("SHOW TABLES").await.expect("show tables");
        let have = rs.iter().any(|r| matches!(r.get::<MVal, _>(0), Some(MVal::Bytes(b)) if String::from_utf8_lossy(&b) == table));
        if have {
            return;
        }
        assert!(Instant::now() < deadline, "table {table} never replicated");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Sorted first-column strings.
async fn col_sorted(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    let mut out: Vec<String> = rs
        .into_iter()
        .map(|r| match r.get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
            Some(MVal::Int(i)) => i.to_string(),
            Some(MVal::Double(d)) => d.to_string(),
            v => panic!("cell {v:?} for {sql}"),
        })
        .collect();
    out.sort();
    out
}

async fn start_sql_cluster(dir: &Path) -> Vec<ProcNode> {
    let mut nodes = Vec::new();
    let mut first = spawn_node_sql(dir, 0, true, None);
    wait_resp_ready(&mut first, 30).await;
    wait_mysql_ready(&first, 15).await;
    nodes.push(first);
    assert_eq!(wait_leader(&nodes, 60).await, 0, "node0 must lead first");
    let join = nodes[0].http.clone();
    for id in 1..3 {
        let mut node = spawn_node_sql(dir, id, false, Some(&join));
        wait_resp_ready(&mut node, 30).await;
        wait_mysql_ready(&node, 15).await;
        nodes.push(node);
    }
    let leader = wait_leader(&nodes, 60).await;
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 30).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reg = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"raft", b"get", b"sql_nodes"]).await;
        let ready = binds
            .iter()
            .all(|b| common::contains_bytes(&reg, b.as_bytes()))
            && !common::contains_bytes(&reg, b"\"sql_rpc\":\"\"");
        if ready {
            return nodes;
        }
        assert!(
            Instant::now() < deadline,
            "registry never converged: {reg:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

/// Sorted first-column strings from every node for the same compound
/// query: cs holds ids 1..=15 (5 per node, distinct ranges), rs holds
/// 100..=104 (one 2PC insert scattered to slot-band owners).
#[tokio::test]
async fn union_and_cte_gather_cluster_wide() {
    let dir = std::env::temp_dir().join(format!("rdb-setop-cluster-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut nodes = start_sql_cluster(&dir).await;
    let leader = wait_leader(&nodes, 60).await;
    let mut conns = Vec::new();
    for n in &nodes {
        conns.push(connect(n).await);
    }

    ddl(
        &mut conns[leader],
        "CREATE TABLE cs (id BIGINT PRIMARY KEY, v VARCHAR(8) NULL) ENGINE=columnar",
    )
    .await;
    ddl(
        &mut conns[leader],
        "CREATE TABLE rs (id BIGINT PRIMARY KEY, w VARCHAR(8) NULL)",
    )
    .await;
    for conn in conns.iter_mut() {
        poll_catalog(conn, "cs").await;
        poll_catalog(conn, "rs").await;
    }

    // Columnar: 5 DISTINCT ids committed on each node (local
    // segments, ids never overlap so the gather is exactly 15).
    for (i, conn) in conns.iter_mut().enumerate() {
        let lo = i as i64 * 5 + 1;
        let rows: Vec<String> = (0..5)
            .map(|j| format!("({}, 'c{}')", lo + j, lo + j))
            .collect();
        conn.query_drop(format!("INSERT INTO cs (id, v) VALUES {}", rows.join(", ")))
            .await
            .expect("columnar seed");
    }
    // Row store: ONE insert of 5 ids anywhere (2PC scatters them to
    // their slot-band owners).
    conns[0]
        .query_drop(
            "INSERT INTO rs (id, w) VALUES (100,'x'),(101,'y'),(102,'z'),(103,'p'),(104,'q')",
        )
        .await
        .expect("row seed");

    // Read-point convergence: lift every oracle view above all
    // columnar commits (4200 rows > one 4096-row ts block per node).
    ddl(
        &mut conns[leader],
        "CREATE TABLE adv (k BIGINT PRIMARY KEY)",
    )
    .await;
    for conn in conns.iter_mut() {
        poll_catalog(conn, "adv").await;
    }
    for (i, conn) in conns.iter_mut().enumerate() {
        let base = (i as i64 + 1) * 1_000_000;
        let rows: Vec<String> = (0..4200)
            .map(|j| format!("({})", base + j as i64))
            .collect();
        conn.query_drop(format!("INSERT INTO adv (k) VALUES {}", rows.join(", ")))
            .await
            .expect("ts-advancer insert");
    }

    // UNION distinct over columnar + row operands: 20 ids, identical
    // from every node.
    let mut expect: Vec<String> = (1..=15).chain(100..=104).map(|i| i.to_string()).collect();
    expect.sort();
    for (i, conn) in conns.iter_mut().enumerate() {
        let got = col_sorted(conn, "SELECT id FROM cs UNION SELECT id FROM rs").await;
        assert_eq!(got, expect, "node{i} union gather");
    }

    // UNION ALL keeps duplicates: 15 + 5 literal rows = 20.
    for (i, conn) in conns.iter_mut().enumerate() {
        let got = col_sorted(conn, "SELECT id FROM cs UNION ALL SELECT 100 FROM rs").await;
        assert_eq!(got.len(), 20, "node{i} union all len");
        assert_eq!(
            got.iter().filter(|v| *v == "100").count(),
            5,
            "node{i} union all dups"
        );
    }

    // CTE over a gathered columnar operand, then UNION ALL of the
    // two counts (13..=15 -> 3, rs -> 5).
    for (i, conn) in conns.iter_mut().enumerate() {
        let got = col_sorted(
            conn,
            "WITH big AS (SELECT id FROM cs WHERE id > 12) \
             SELECT COUNT(*) FROM big UNION ALL SELECT COUNT(*) FROM rs",
        )
        .await;
        assert_eq!(got, vec!["3".to_string(), "5".to_string()], "node{i} cte");
    }

    for n in &mut nodes {
        n.kill_now();
    }
}
