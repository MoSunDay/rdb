//! M4 end-to-end: the columnar table engine over REAL rdb processes.
//! Single node: ENGINE=columnar DDL, append-only INSERTs (multi-row,
//! NULLs), SELECT/WHERE/COUNT/SUM over segments, explicit-txn staged
//! visibility + rollback, the append-only rejections (UPDATE/DELETE/
//! CREATE INDEX -> MySQL 1235), DROP TABLE cleanup and table-id reuse.
//! Cluster (3 nodes): each node's INSERTs commit their own segment; a
//! large row-store 2PC INSERT per node lifts every member's ts read
//! point above all commits, then reads from ANY node fan out
//! ("Gather(columnar") and return the union exactly once.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{
    cluster_init, cmd_one_shot, spawn_node_mysql, spawn_node_sql, wait_cluster_nodes_list_all,
    wait_leader, wait_mysql_ready, wait_resp_ready, ProcNode, TOKEN,
};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";
/// MySQL errno of `ErrorCode::NotSupported` (ER_NOT_SUPPORTED_YET).
const ER_NOT_SUPPORTED_YET: u16 = 1235;
/// MySQL errno of `ErrorCode::NoSuchTable` (ER_NO_SUCH_TABLE).
const ER_NO_SUCH_TABLE: u16 = 1146;

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

/// Poll SHOW TABLES until the raft-replicated catalog reaches `want`
/// (true = table listed, false = dropped everywhere).
async fn poll_catalog(conn: &mut mysql_async::Conn, table: &str, node: &ProcNode, want: bool) {
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
            "table {table} never reached want={want} on {} (have {names:?})",
            node.resp
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// All cells of a resultset as strings (NULL as "NULL").
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

/// Run `sql` and require a server error with exactly `errno`.
async fn assert_errno(conn: &mut mysql_async::Conn, sql: &str, errno: u16) {
    let err = conn.query_drop(sql).await.expect_err(sql);
    match err {
        mysql_async::Error::Server(e) if e.code == errno => {}
        other => panic!("{sql}: expected MySQL errno {errno}, got {other}"),
    }
}

#[tokio::test]
async fn single_node_columnar_lifecycle() {
    let dir = std::env::temp_dir().join(format!("rdb-columnar-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;
    let mut c2 = connect(&node).await;

    // ---- columnar DDL ----
    ddl(
        &mut c,
        "CREATE TABLE ct (id BIGINT PRIMARY KEY, name VARCHAR(64) NULL, \
         score DOUBLE NOT NULL, ok BOOLEAN NOT NULL, blob_col BLOB NULL) ENGINE=columnar",
    )
    .await;

    // ---- append-only inserts: one multi-row statement (with a NULL
    // name) plus separate statements, each its own segment ----
    c.query_drop(
        "INSERT INTO ct (id, name, score, ok, blob_col) VALUES \
         (1, 'ada', 9.5, 1, 'b1'), (2, NULL, 3.25, 0, NULL), (3, 'bob', 7.0, 1, 'b3')",
    )
    .await
    .expect("batch insert");
    c.query_drop("INSERT INTO ct (id, name, score, ok, blob_col) VALUES (4, 'dee', 3.25, 1, 'b4')")
        .await
        .expect("insert 4");
    c.query_drop(
        "INSERT INTO ct (id, name, score, ok, blob_col) VALUES (5, 'eve', 10.75, 0, NULL)",
    )
    .await
    .expect("insert 5");

    // ---- SELECT * / WHERE / aggregates over the union of segments ----
    let mut all = grid(&mut c, "SELECT * FROM ct").await;
    all.sort_by(|a, b| a[0].cmp(&b[0]));
    assert_eq!(
        all,
        vec![
            vec!["1", "ada", "9.5", "1", "b1"],
            vec!["2", "NULL", "3.25", "0", "NULL"],
            vec!["3", "bob", "7", "1", "b3"],
            vec!["4", "dee", "3.25", "1", "b4"],
            vec!["5", "eve", "10.75", "0", "NULL"],
        ]
    );
    assert_eq!(
        col(&mut c, "SELECT id FROM ct WHERE score > 5.0 ORDER BY id").await,
        vec!["1", "3", "5"]
    );
    assert_eq!(
        grid(&mut c, "SELECT COUNT(*), SUM(score) FROM ct").await,
        vec![vec!["5", "33.75"]]
    );

    // ---- explicit txn: staged rows visible to the writer only, then
    // COMMIT flushes them into a segment both connections see ----
    c.query_drop("BEGIN").await.expect("begin");
    c.query_drop(
        "INSERT INTO ct (id, name, score, ok, blob_col) VALUES \
         (6, 'fox', 1.25, 1, NULL), (7, 'gia', 2.25, 0, 'b7')",
    )
    .await
    .expect("staged insert");
    assert_eq!(
        col(&mut c, "SELECT COUNT(*) FROM ct").await,
        vec!["7"],
        "own-write visibility inside the txn"
    );
    assert_eq!(
        col(&mut c2, "SELECT COUNT(*) FROM ct").await,
        vec!["5"],
        "other connection must not see staged rows"
    );
    c.query_drop("COMMIT").await.expect("commit");
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ct").await, vec!["7"]);
    assert_eq!(col(&mut c2, "SELECT COUNT(*) FROM ct").await, vec!["7"]);

    // ---- ROLLBACK discards the staged appends on both connections ----
    c.query_drop("BEGIN").await.expect("begin");
    c.query_drop("INSERT INTO ct (id, name, score, ok, blob_col) VALUES (100, 'x', 1.0, 1, NULL)")
        .await
        .expect("staged insert");
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ct").await, vec!["8"]);
    assert_eq!(col(&mut c2, "SELECT COUNT(*) FROM ct").await, vec!["7"]);
    c.query_drop("ROLLBACK").await.expect("rollback");
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ct").await, vec!["7"]);
    assert_eq!(col(&mut c2, "SELECT COUNT(*) FROM ct").await, vec!["7"]);

    // ---- append-only: UPDATE / DELETE / CREATE INDEX all 1235 ----
    assert_errno(
        &mut c,
        "UPDATE ct SET score = 1.0 WHERE id = 1",
        ER_NOT_SUPPORTED_YET,
    )
    .await;
    assert_errno(&mut c, "DELETE FROM ct WHERE id = 1", ER_NOT_SUPPORTED_YET).await;
    assert_errno(&mut c, "CREATE INDEX i ON ct (name)", ER_NOT_SUPPORTED_YET).await;

    // ---- DROP cleans up; the table id is reusable afterwards ----
    c.query_drop("DROP TABLE ct").await.expect("drop");
    assert_errno(&mut c, "SELECT * FROM ct", ER_NO_SUCH_TABLE).await;
    assert_errno(&mut c, "DROP TABLE ct", ER_NO_SUCH_TABLE).await;
    c.query_drop("DROP TABLE IF EXISTS ct")
        .await
        .expect("if exists");

    // Recreated table gets the SAME id; stale segments were purged, so
    // only the new row comes back.
    ddl(
        &mut c,
        "CREATE TABLE ct (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL) ENGINE=columnar",
    )
    .await;
    c.query_drop("INSERT INTO ct (id, v) VALUES (1, 'zed')")
        .await
        .expect("re-insert");
    assert_eq!(
        grid(&mut c, "SELECT * FROM ct").await,
        vec![vec!["1", "zed"]]
    );
    node.kill_now();
}

/// 3-node SQL cluster with a converged topology and `sql_nodes`
/// registry (same bring-up as sql_dist_read_e2e).
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

#[tokio::test]
async fn cluster_columnar_segments_spread_and_gather() {
    let dir = std::env::temp_dir().join(format!("rdb-columnar-cluster-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir).await;
    let leader = wait_leader(&nodes, 60).await;
    let mut conns = Vec::new();
    for n in &nodes {
        conns.push(connect(n).await);
    }

    // ---- columnar DDL on the raft leader, replicated everywhere ----
    ddl(
        &mut conns[leader],
        "CREATE TABLE cd (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL) ENGINE=columnar",
    )
    .await;
    for (i, n) in nodes.iter().enumerate() {
        poll_catalog(&mut conns[i], "cd", n, true).await;
    }

    // ---- autocommit INSERTs DIRECTLY on every node: each commit
    // flushes its own local segment, so segments spread across nodes ----
    for (i, conn) in conns.iter_mut().enumerate() {
        let lo = (i * 5 + 1) as i64;
        let rows: Vec<String> = (0..5)
            .map(|j| format!("({}, 'n{}')", lo + j, lo + j))
            .collect();
        conn.query_drop(format!("INSERT INTO cd (id, v) VALUES {}", rows.join(", ")))
            .await
            .expect("per-node insert");
    }

    // ---- read-point convergence: a columnar commit advances only the
    // committing node's ts knowledge, so a member's snapshot read point
    // can sit BELOW segments committed elsewhere. A row-store 2PC INSERT
    // larger than one ts block (4096) forces a watermark above every
    // earlier grant; Decide lifts every participant, one INSERT per node.
    ddl(
        &mut conns[leader],
        "CREATE TABLE adv (k BIGINT PRIMARY KEY)",
    )
    .await;
    for (i, n) in nodes.iter().enumerate() {
        poll_catalog(&mut conns[i], "adv", n, true).await;
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

    // ---- reads from EVERY node fan out and return the union ----
    let expect: Vec<String> = (1..=15).map(|i| i.to_string()).collect();
    for (i, conn) in conns.iter_mut().enumerate() {
        assert_eq!(
            col(conn, "SELECT id FROM cd ORDER BY id").await,
            expect,
            "node {i} must gather every segment exactly once"
        );
        assert_eq!(
            col(conn, "SELECT COUNT(*) FROM cd").await,
            vec!["15"],
            "node {i} count"
        );
    }
    assert_eq!(
        col(&mut conns[0], "SELECT id FROM cd WHERE v = 'n7'").await,
        vec!["7"]
    );
    assert_eq!(
        col(&mut conns[1], "SELECT id FROM cd WHERE id > 10 ORDER BY id").await,
        vec!["11", "12", "13", "14", "15"]
    );

    // ---- EXPLAIN announces the columnar fan-out ----
    let plan = col(&mut conns[2], "EXPLAIN SELECT * FROM cd").await;
    assert!(
        plan[0].starts_with("Gather(columnar"),
        "plan headline: {plan:?}"
    );

    // ---- DROP on the leader hides the table on every node ----
    conns[leader]
        .query_drop("DROP TABLE cd")
        .await
        .expect("drop");
    for (i, n) in nodes.iter().enumerate() {
        poll_catalog(&mut conns[i], "cd", n, false).await;
        assert_errno(&mut conns[i], "SELECT * FROM cd", ER_NO_SUCH_TABLE).await;
    }
    for mut n in nodes {
        n.kill_now();
    }
}
