//! Regression: a raft-acked DDL must be VISIBLE on every node when the
//! ack reaches the client.
//!
//! `CREATE UNIQUE INDEX` used to return once the leader had applied the
//! catalog entry; followers apply asynchronously, so a metadata read
//! (SHOW INDEX) or a schema-planning write issued on another node right
//! after the ack could still see the pre-DDL catalog -- the fresh
//! unique index was invisible and follower-coordinated writes were not
//! vetoed against it. DDL now holds its response until every reachable
//! peer's FSM serves the mutation (`storage::replicate`), so the reads
//! below are immediate and unconditioned: no poll, no sleep.

mod common;

use std::time::{Duration, Instant};

use common::{start_sql_cluster, ProcNode};
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

/// DDL needs the raft leader; retry until the bootstrap node becomes one.
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
                panic!("ddl {sql}: {e}");
            }
        }
    }
}

/// Poll SHOW TABLES until the catalog reaches the follower. Only the
/// TABLE existence is awaited (the pre-DDL baseline); the unique INDEX
/// visibility is then asserted IMMEDIATELY after the DDL ack.
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Key_name cells of SHOW INDEX (third column: Table, Non_unique, Key_name...).
async fn index_names(conn: &mut mysql_async::Conn) -> Vec<String> {
    let rs: Vec<mysql_async::Row> = conn
        .query("SHOW INDEX FROM orders_t")
        .await
        .expect("show index");
    rs.into_iter()
        .map(|r| match r.get::<MVal, _>(2) {
            Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap(),
            v => panic!("non-bytes key_name cell {v:?}"),
        })
        .collect()
}

#[tokio::test]
async fn unique_index_visible_and_enforced_on_followers_immediately() {
    let dir = std::env::temp_dir().join(format!("rdb-ddl-visibility-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let mut c0 = connect(&nodes[0]).await; // bootstrap raft leader
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    ddl(
        &mut c0,
        "CREATE TABLE orders_t (id BIGINT PRIMARY KEY, user_id VARCHAR(64) NOT NULL)",
    )
    .await;
    wait_table(&mut c1, "orders_t", &nodes[1]).await;
    wait_table(&mut c2, "orders_t", &nodes[2]).await;
    // Baseline rows; 'u1' becomes the value the dup insert reuses.
    c0.query_drop("INSERT INTO orders_t (id, user_id) VALUES (1, 'u1')")
        .await
        .expect("seed row");

    // The DDL ack must imply cluster-wide catalog visibility: the
    // assertions below run on BOTH followers immediately, unpolled.
    ddl(&mut c0, "CREATE UNIQUE INDEX uk_user ON orders_t (user_id)").await;

    for (c, label) in [(&mut c1, "node1"), (&mut c2, "node2")] {
        let names = index_names(c).await;
        assert!(
            names.iter().any(|n| n == "uk_user"),
            "uk_user missing from SHOW INDEX on {label} right after the DDL ack: {names:?}"
        );
    }

    // Consequence: a follower-coordinated insert reusing 'u1' must be
    // vetoed (the coordinator plans unique entries from ITS catalog
    // view, so a stale view would silently skip the enforcement).
    let err = c2
        .query_drop("INSERT INTO orders_t (id, user_id) VALUES (2, 'u1')")
        .await
        .expect_err("dup insert must be rejected");
    assert!(
        err.to_string().contains("Duplicate entry") || err.to_string().contains("dup:"),
        "unexpected error: {err}"
    );

    // Same visibility contract for DROP: no follower lists the table
    // once the DROP ack returns.
    ddl(&mut c0, "DROP TABLE orders_t").await;
    for (c, label) in [(&mut c1, "node1"), (&mut c2, "node2")] {
        let rs: Vec<mysql_async::Row> = c.query("SHOW TABLES").await.expect("show tables");
        let names: Vec<String> = rs
            .into_iter()
            .map(|r| match r.get::<MVal, _>(0) {
                Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap(),
                v => panic!("non-bytes table cell {v:?}"),
            })
            .collect();
        assert!(
            !names.iter().any(|n| n == "orders_t"),
            "orders_t still listed on {label} right after the DROP ack: {names:?}"
        );
    }

    for mut n in nodes {
        n.kill_now();
    }
}
