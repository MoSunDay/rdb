//! M3 end-to-end: scatter-gather distributed SELECTs over three REAL
//! rdb processes. Each node stores only its slot band, so a SELECT
//! anywhere must fan out: coordinator scans its own band and pulls the
//! other two through ScanBand RPCs, then runs filter/aggregate/order
//! locally over the merged rows. Covered behavior:
//! - `SELECT *` from EVERY node returns the full table, exactly once
//!   per row (bands are disjoint, pk -> slot is pure);
//! - WHERE / ORDER BY / LIMIT / COUNT / SUM / GROUP BY all compute
//!   over the gathered union;
//! - repeatable read holds ACROSS the gather: an explicit txn on one
//!   node keeps its pinned snapshot even after another node commits a
//!   spanning INSERT (every participant's oracle advanced), and sees
//!   the rows after COMMIT;
//! - EXPLAIN announces the distributed plan ("Gather(bands=3)").

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
/// Rows per INSERT batch: 40 ids over 3 slot bands cannot fit on one
/// node, so every node is a 2PC participant (and advances its read
/// point on Decide), which is what makes the snapshot checks below
/// deterministic.
const BATCH: i64 = 40;

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
            Err(e) => {
                assert!(Instant::now() < deadline, "mysql connect: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// DDL retries while it is forwarded to the leader and the raft log
/// has not caught up on the executing node yet.
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

/// Poll SHOW TABLES on one node until the raft-replicated catalog
/// reaches it (DDL lands on the leader first).
async fn wait_table(conn: &mut mysql_async::Conn, table: &str, node: &ProcNode) {
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
        if names.iter().any(|n| n == table) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "table {table} never reached {} (have {names:?})",
            node.resp
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 3-node SQL cluster with a converged topology and `sql_nodes`
/// registry (sql_rpc ports resolvable: the precondition for both the
/// 2PC write path and the ScanBand read path).
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

/// All cells of a resultset as strings (NULL as "NULL"): ids come back
/// as MySQL bytes, ints as ints -- normalize both.
async fn grid(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<String>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            let cols = r.columns_ref().to_vec();
            let raw = r.unwrap_raw();
            cols.iter()
                .zip(raw)
                .map(|(_, v)| match v {
                    Some(MVal::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Some(MVal::Int(i)) => i.to_string(),
                    Some(MVal::Double(d)) => d.to_string(),
                    Some(MVal::NULL) | None => "NULL".to_string(),
                    Some(other) => format!("{other:?}"),
                })
                .collect()
        })
        .collect()
}

async fn col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    grid(conn, sql)
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

fn batch_sql(lo: i64) -> String {
    let rows: Vec<String> = (0..BATCH)
        .map(|i| {
            format!(
                "({}, 'n{}', '{}')",
                lo + i,
                lo + i,
                if i % 2 == 0 { "even" } else { "odd" }
            )
        })
        .collect();
    format!(
        "INSERT INTO items (id, name, tag) VALUES {}",
        rows.join(", ")
    )
}

#[tokio::test]
async fn distributed_reads_gather_filter_aggregate_and_snapshot() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-dist-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR(128) NOT NULL, \
         tag VARCHAR(8) NOT NULL)",
    )
    .await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    // One spanning batch through node0: rows land on all three slot
    // owners, so every subsequent read must gather remote bands.
    c0.query_drop(batch_sql(1)).await.expect("spanning insert");

    // ---- SELECT * from every node sees the whole table ----
    let expect: Vec<String> = (1..=BATCH).map(|i| i.to_string()).collect();
    for (i, conn) in [&mut c0, &mut c1, &mut c2].into_iter().enumerate() {
        let got = col(conn, "SELECT id FROM items ORDER BY id").await;
        assert_eq!(got, expect, "node {i} must gather all bands");
    }

    // ---- WHERE / ORDER BY DESC / LIMIT over gathered rows ----
    assert_eq!(
        col(
            &mut c1,
            "SELECT id FROM items WHERE id > 37 ORDER BY id DESC"
        )
        .await,
        vec!["40", "39", "38"]
    );
    assert_eq!(
        col(
            &mut c2,
            "SELECT id FROM items WHERE tag = 'odd' ORDER BY id LIMIT 3"
        )
        .await,
        vec!["2", "4", "6"]
    );

    // ---- aggregates computed on the coordinator ----
    assert_eq!(
        grid(&mut c1, "SELECT COUNT(*), SUM(id) FROM items").await,
        vec![vec!["40", "820"]]
    );
    assert_eq!(
        grid(
            &mut c2,
            "SELECT tag, COUNT(*) AS c FROM items GROUP BY tag ORDER BY tag"
        )
        .await,
        vec![vec!["even", "20"], vec!["odd", "20"]]
    );

    // ---- EXPLAIN announces the distributed plan ----
    let plan = col(&mut c1, "EXPLAIN SELECT id FROM items WHERE id > 5").await;
    assert!(plan[0].starts_with("Gather(bands=3)"), "plan: {plan:?}");
    assert_eq!(plan[1], "SeqScan items");

    // ---- repeatable read holds across the gather ----
    c1.query_drop("BEGIN").await.expect("begin");
    assert_eq!(
        col(&mut c1, "SELECT COUNT(*) FROM items").await,
        vec!["40"],
        "txn snapshot before the concurrent write"
    );
    // A second spanning batch commits through node0 while node1's txn
    // is open: node1 is a participant and advances its read point, but
    // the txn stays pinned at its own read_ts.
    c0.query_drop(batch_sql(1000)).await.expect("second insert");
    assert_eq!(
        col(&mut c1, "SELECT COUNT(*) FROM items").await,
        vec!["40"],
        "pinned snapshot must not see the concurrent commit"
    );
    c1.query_drop("COMMIT").await.expect("commit");
    assert_eq!(
        col(&mut c1, "SELECT COUNT(*) FROM items").await,
        vec!["80"],
        "post-commit read gathers the new rows"
    );
}
