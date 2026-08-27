//! P0 regression e2e: cluster-mode JOINs must not silently read only
//! the local node's slice of each side. Columnar segments commit where
//! the txn closed and row-store rows land in per-slot bands, so a JOIN
//! executed on ANY node has to gather BOTH sides cluster-wide before
//! the nested loop runs. Covered:
//! - columnar JOIN row-store: segments spread over 3 nodes, every node
//!   returns the full pair set exactly once;
//! - row JOIN row (self-join with aliases): band-spread rows, every
//!   node sees all pairs;
//! - EXPLAIN announces the gather for join trees.

mod common;

use std::time::{Duration, Instant};

use common::{start_sql_cluster, wait_leader, ProcNode};
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

/// DDL needs the raft leader; retry while the executing node forwards.
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

/// Poll SHOW TABLES until the raft-replicated catalog reaches `want`.
async fn poll_catalog(conn: &mut mysql_async::Conn, table: &str, want: bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let rs: Vec<mysql_async::Row> = conn.query("SHOW TABLES").await.expect("show tables");
        let names: Vec<String> = rs
            .into_iter()
            .map(|r| match r.get::<MVal, _>(0) {
                Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap(),
                v => panic!("non-bytes table cell {v:?}"),
            })
            .collect();
        if names.iter().any(|n| n == table) == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "table {table} never reached want={want} (have {names:?})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// First column of a resultset as strings.
async fn col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| match r.get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
            Some(MVal::Int(i)) => i.to_string(),
            v => panic!("non-string cell {v:?} for {sql}"),
        })
        .collect()
}

#[tokio::test]
async fn cluster_joins_gather_both_sides() {
    let dir = std::env::temp_dir().join(format!("rdb-join-cluster-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let leader = wait_leader(&nodes, 60).await;
    let mut conns = Vec::new();
    for n in &nodes {
        conns.push(connect(n).await);
    }

    // ---- schemas: one columnar, one row-store, same id domain 1..=15
    ddl(
        &mut conns[leader],
        "CREATE TABLE cj (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL) ENGINE=columnar",
    )
    .await;
    ddl(
        &mut conns[leader],
        "CREATE TABLE rj (id BIGINT PRIMARY KEY, w VARCHAR(64) NULL)",
    )
    .await;
    for (i, n) in nodes.iter().enumerate() {
        let _ = n;
        poll_catalog(&mut conns[i], "cj", true).await;
        poll_catalog(&mut conns[i], "rj", true).await;
    }

    // ---- columnar segments: 5 rows committed DIRECTLY on every node
    // (each commit flushes a local segment, so segments spread).
    for (i, conn) in conns.iter_mut().enumerate() {
        let lo = (i * 5 + 1) as i64;
        let rows: Vec<String> = (0..5)
            .map(|j| format!("({}, 'c{}')", lo + j, lo + j))
            .collect();
        conn.query_drop(format!("INSERT INTO cj (id, v) VALUES {}", rows.join(", ")))
            .await
            .expect("per-node columnar insert");
    }
    // ---- row-store band spread: 300 fixed ids in ONE insert on node0
    // (2PC distributes them to their slot-band owners).
    let rows: Vec<String> = (1..=300).map(|k| format!("({k}, 'w{k}')")).collect();
    conns[0]
        .query_drop(format!("INSERT INTO rj (id, w) VALUES {}", rows.join(", ")))
        .await
        .expect("row-store spread insert");
    // ids 1..=15 into rj as the join partner set: reuse of the same
    // table keeps the partner rows inside the 300 (ids 1..=15 exist).

    // ---- read-point convergence: a row-store 2PC insert larger than
    // one ts block (4096) per node lifts every oracle view above all
    // columnar commits (same trick as the columnar gather e2e).
    ddl(
        &mut conns[leader],
        "CREATE TABLE adv (k BIGINT PRIMARY KEY)",
    )
    .await;
    for (i, n) in nodes.iter().enumerate() {
        let _ = n;
        poll_catalog(&mut conns[i], "adv", true).await;
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

    let expect_ids: Vec<String> = (1..=15).map(|i| i.to_string()).collect();
    for (i, conn) in conns.iter_mut().enumerate() {
        // columnar JOIN row: every node must see ALL 15 pairs exactly
        // once (before the fix a node saw only its own 5 segments).
        assert_eq!(
            col(
                conn,
                "SELECT cj.id FROM cj JOIN rj ON cj.id = rj.id ORDER BY cj.id"
            )
            .await,
            expect_ids,
            "node {i}: mixed-engine join must gather the columnar side"
        );
        assert_eq!(
            col(conn, "SELECT COUNT(*) FROM cj JOIN rj ON cj.id = rj.id").await,
            vec!["15"],
            "node {i}: join count"
        );
        // row JOIN row (self-join): every node must see all 300 rows
        // through the band gather (before the fix only the local band).
        assert_eq!(
            col(conn, "SELECT COUNT(*) FROM rj a JOIN rj b ON a.id = b.id").await,
            vec!["300"],
            "node {i}: row-store join must gather both bands"
        );
        // row-side predicate + join: partner row filter still applies.
        assert_eq!(
            col(
                conn,
                "SELECT COUNT(*) FROM cj JOIN rj ON cj.id = rj.id WHERE rj.w = 'w7'"
            )
            .await,
            vec!["1"],
            "node {i}: join with WHERE"
        );
    }

    // ---- EXPLAIN announces the gather over join trees ----
    let plan = col(
        &mut conns[2],
        "EXPLAIN SELECT * FROM cj JOIN rj ON cj.id = rj.id",
    )
    .await;
    assert_eq!(plan[0], "Gather(join)", "plan headline: {plan:?}");

    for mut n in nodes {
        n.kill_now();
    }
}
