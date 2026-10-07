//! M2 DML-conflict e2e (part 3, cluster) over a REAL 3-process rdb
//! SQL cluster (see `plans/2026-10-06-mysql-gap/m2-dml-conflicts.md`,
//! decision point 1, choice (b)): ODKU and REPLACE need the CONFLICTING
//! existing row at decide time and the conflict read is local-band
//! only, so in cluster mode they reject loudly with MySQL 1235 naming
//! "cluster mode" -- from every node, before anything lands. The
//! cluster-safe plain paths stay intact: a band-spanning
//! `INSERT ... SELECT` commits through 2PC, the unique index still
//! vetoes duplicate values, and the plain-INSERT pk upsert works.

mod common;

use std::time::{Duration, Instant};

use common::{start_sql_cluster, wait_leader, ProcNode};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";
/// MySQL 1062 (ER_DUP_ENTRY).
const ER_DUP_ENTRY: u16 = 1062;
/// MySQL 1235 (ER_NOT_SUPPORTED_YET).
const ER_NOT_SUPPORTED_YET: u16 = 1235;
/// Rows in the band-spanning batches (~(1/3)^N chance of landing on a
/// single node, same shape as the M3 2PC e2e).
const BATCH: i64 = 30;

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

/// Poll SHOW TABLES on one node until the raft-replicated catalog
/// reaches it (DDL lands on the leader first).
async fn wait_table(conn: &mut mysql_async::Conn, table: &str) {
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
            "table {table} never reached this node (have {names:?})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// One multi-row INSERT of BATCH rows (distinct ids and names).
fn batch_sql(lo: i64) -> String {
    let rows: Vec<String> = (0..BATCH)
        .map(|i| format!("({}, 'n{}')", lo + i, lo + i))
        .collect();
    format!("INSERT INTO cs (id, name) VALUES {}", rows.join(", "))
}

/// The server error a statement failed with.
async fn server_error(conn: &mut mysql_async::Conn, sql: &str) -> mysql_async::ServerError {
    match conn.query::<mysql_async::Row, _>(sql).await {
        Err(mysql_async::Error::Server(e)) => e,
        other => panic!("expected server error for {sql}, got {other:?}"),
    }
}

#[tokio::test]
async fn cluster_rejects_conflict_paths_but_plain_writes_work() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-upsert-cluster-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let leader = wait_leader(&nodes, 60).await;
    let mut conns = Vec::new();
    for n in &nodes {
        conns.push(connect(n).await);
    }

    // ---- schemas: two row tables, ci with a unique name index ----
    ddl(
        &mut conns[leader],
        "CREATE TABLE ci (id BIGINT PRIMARY KEY, name VARCHAR(64) NOT NULL)",
    )
    .await;
    ddl(
        &mut conns[leader],
        "CREATE TABLE cs (id BIGINT PRIMARY KEY, name VARCHAR(64) NOT NULL)",
    )
    .await;
    ddl(
        &mut conns[leader],
        "CREATE UNIQUE INDEX ci_name ON ci (name)",
    )
    .await;
    for c in conns.iter_mut().skip(1) {
        wait_table(c, "ci").await;
        wait_table(c, "cs").await;
    }

    // ---- decision 1b: ODKU / REPLACE / ODKU-on-SELECT reject loudly
    // from EVERY node (each is a forwarding boundary), 1235 naming
    // cluster mode, and nothing lands ----
    for (node, sql) in [
        (0, "INSERT INTO ci (id, name) VALUES (1, 'a') ON DUPLICATE KEY UPDATE name = VALUES(name)"),
        (2, "INSERT INTO ci (id, name) VALUES (1, 'a') ON DUPLICATE KEY UPDATE name = VALUES(name)"),
        (1, "REPLACE INTO ci (id, name) VALUES (1, 'a')"),
        (0, "INSERT INTO ci (id, name) SELECT id, name FROM cs ON DUPLICATE KEY UPDATE name = VALUES(name)"),
        (2, "REPLACE INTO ci (id, name) SELECT id, name FROM cs"),
    ] {
        let e = server_error(&mut conns[node], sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "node {node} {sql}: {e}");
        assert!(
            e.message.contains("not supported in cluster mode"),
            "node {node} {sql}: {e}"
        );
    }
    for c in conns.iter_mut() {
        let rs: Vec<mysql_async::Row> = c.query("SELECT COUNT(*) FROM ci").await.expect("count ci");
        assert_eq!(rs[0].get::<i64, _>(0).unwrap(), 0, "no row may land");
    }

    // ---- the plain paths keep working: a band-spanning INSERT into
    // cs, then a plain INSERT..SELECT ci <- cs through 2PC ----
    conns[0]
        .query_drop(batch_sql(1))
        .await
        .expect("spanning insert into cs");
    conns[1]
        .query_drop("INSERT INTO ci (id, name) SELECT id, name FROM cs")
        .await
        .expect("plain INSERT..SELECT across cluster tables");
    assert_eq!(conns[1].affected_rows() as i64, BATCH);
    for (i, c) in conns.iter_mut().enumerate() {
        let rs: Vec<mysql_async::Row> = c
            .query("SELECT COUNT(*), MIN(id), MAX(id) FROM ci")
            .await
            .expect("gathered count");
        let (n, lo, hi): (i64, i64, i64) = (
            rs[0].get(0).unwrap(),
            rs[0].get(1).unwrap(),
            rs[0].get(2).unwrap(),
        );
        assert_eq!(
            (n, lo, hi),
            (BATCH, 1, BATCH),
            "node {i} gathered read after INSERT..SELECT"
        );
    }

    // ---- unique index still vets across bands: a duplicate name is
    // 1062 and leaves the original rows intact everywhere ----
    let e = server_error(
        &mut conns[0],
        "INSERT INTO ci (id, name) VALUES (9999, 'n5')",
    )
    .await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e}");
    let rs: Vec<mysql_async::Row> = conns[2]
        .query("SELECT id FROM ci WHERE name = 'n5'")
        .await
        .expect("owner of n5");
    assert_eq!(rs[0].get::<i64, _>(0).unwrap(), 5);

    // ---- plain-INSERT pk upsert (the intentional deviation) still
    // rides 2PC: same pk rewrites through, gathered read sees it ----
    conns[0]
        .query_drop("INSERT INTO ci (id, name) VALUES (5, 'renamed')")
        .await
        .expect("pk upsert through 2PC");
    for (i, c) in conns.iter_mut().enumerate() {
        let rs: Vec<mysql_async::Row> = c
            .query("SELECT name FROM ci WHERE id = 5")
            .await
            .expect("gathered read of the upserted pk");
        let name = match rs[0].get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap(),
            v => panic!("node {i}: non-bytes cell {v:?}"),
        };
        assert_eq!(name, "renamed", "node {i}");
    }
    for mut n in nodes {
        n.kill_now();
    }
}
