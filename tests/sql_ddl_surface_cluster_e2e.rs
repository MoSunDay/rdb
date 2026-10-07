//! M4 DDL surface on a 3-process cluster (`start_sql_cluster`, the
//! pattern of `sql_ddl_visibility_e2e` / `sql_join_cluster_e2e`):
//! TRUNCATE and RENAME are raft-replicated CATALOG effects, so the DDL
//! ack (held until every peer's FSM serves the mutation) must make
//! them visible -- and enforced -- on every follower. Covered:
//! - TRUNCATE: pre-truncate rows unreachable via every node the moment
//!   the ack returns; fresh post-truncate writes visible everywhere,
//!   including re-use of an old unique value (no 1062 anywhere) and
//!   swept old entries that no longer answer index lookups;
//! - RENAME: the new name serves the old data on every node, the old
//!   name 1146s on followers too, SHOW TABLES reflects the swap;
//! - SHOW CREATE TABLE answers identically from every node;
//! - DDL issued on a follower rejects with the leader error (the RESP
//!   control-plane convention; clients retry on the leader).

mod common;

use std::time::{Duration, Instant};

use common::mysql::{connect_root, ddl, rows, wait_table};
use common::start_sql_cluster;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

/// Poll a one-cell SQL until it reads `want`: fresh 2PC writes only
/// become visible once the reader's oracle view passes the commit
/// point, so row reads poll while catalog effects assert immediately.
async fn poll_scalar(conn: &mut mysql_async::Conn, sql: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let got = one_text(conn, sql).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{sql} never reached {want} (last {got})"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// First cell of the first row of a one-value SELECT, as text.
async fn one_text(conn: &mut mysql_async::Conn, sql: &str) -> String {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    match rs.first().and_then(|r| r.get::<MVal, _>(0)) {
        Some(MVal::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
        Some(MVal::Int(i)) => i.to_string(),
        v => panic!("non-scalar cell {v:?} for {sql}"),
    }
}

/// First-column SHOW TABLES cells.
async fn table_names(conn: &mut mysql_async::Conn) -> Vec<String> {
    rows(conn, "SHOW TABLES")
        .await
        .into_iter()
        .map(|mut r| match r.remove(0) {
            MVal::Bytes(b) => String::from_utf8(b).unwrap(),
            v => panic!("non-bytes table cell {v:?}"),
        })
        .collect()
}

#[tokio::test]
async fn truncate_rename_and_show_create_visible_cluster_wide() {
    let dir = std::env::temp_dir().join(format!("rdb-ddl-surface-cl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    // conns[0] is the bootstrap node = the raft leader (the visibility
    // suite's convention); 1 and 2 are followers.
    let mut conns: Vec<mysql_async::Conn> = Vec::new();
    for n in &nodes {
        conns.push(connect_root(n).await);
    }

    ddl(
        &mut conns[0],
        "CREATE TABLE ctr (id BIGINT PRIMARY KEY, u VARCHAR(32) NOT NULL)",
    )
    .await;
    wait_table(&mut conns[1], "ctr", true).await;
    wait_table(&mut conns[2], "ctr", true).await;
    ddl(&mut conns[0], "CREATE UNIQUE INDEX uk_u ON ctr (u)").await;
    conns[0]
        .query_drop("INSERT INTO ctr (id, u) VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .await
        .expect("seed rows");
    // Baseline: every node's gathered read sees all 3 rows.
    for c in &mut conns {
        poll_scalar(c, "SELECT COUNT(*) FROM ctr", "3").await;
    }

    // ---- TRUNCATE: the table-id swap lands with the DDL ack, so the
    // pre-truncate rows are unreachable via EVERY node immediately.
    ddl(&mut conns[0], "TRUNCATE TABLE ctr").await;
    for (i, c) in conns.iter_mut().enumerate() {
        assert_eq!(
            one_text(c, "SELECT COUNT(*) FROM ctr").await,
            "0",
            "node {i}: pre-truncate rows must be unreachable"
        );
    }

    // Fresh writes under the NEW table id, re-using an old unique value
    // (the swept entries must not veto anywhere).
    conns[0]
        .query_drop("INSERT INTO ctr (id, u) VALUES (10, 'a'), (11, 'd')")
        .await
        .expect("fresh post-truncate writes");
    for (i, c) in conns.iter_mut().enumerate() {
        poll_scalar(c, "SELECT COUNT(*) FROM ctr", "2").await;
        assert_eq!(
            one_text(c, "SELECT COUNT(*) FROM ctr WHERE u = 'b'").await,
            "0",
            "node {i}: swept unique value must not answer"
        );
        assert_eq!(
            one_text(c, "SELECT id FROM ctr WHERE u = 'a'").await,
            "10",
            "node {i}: fresh row reachable through the unique index"
        );
    }

    // ---- RENAME: catalog-only, so the data rides along on every node.
    ddl(&mut conns[0], "RENAME TABLE ctr TO ctr2").await;
    for (i, c) in conns.iter_mut().enumerate() {
        poll_scalar(c, "SELECT COUNT(*) FROM ctr2", "2").await;
        let err = c
            .query_drop("SELECT COUNT(*) FROM ctr")
            .await
            .expect_err("the old name must 1146 on followers too");
        assert!(
            err.to_string().contains("1146"),
            "node {i}: old name read: {err}"
        );
        let tables = table_names(c).await;
        assert!(
            tables.iter().any(|t| t == "ctr2") && !tables.iter().any(|t| t == "ctr"),
            "node {i}: SHOW TABLES must reflect the rename, got {tables:?}"
        );
    }

    // ---- SHOW CREATE TABLE answers identically from every node.
    ddl(
        &mut conns[0],
        "CREATE TABLE shape (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, KEY k_v (v))",
    )
    .await;
    wait_table(&mut conns[1], "shape", true).await;
    wait_table(&mut conns[2], "shape", true).await;
    let mut renders = Vec::new();
    for (i, c) in conns.iter_mut().enumerate() {
        let cell = rows(c, "SHOW CREATE TABLE shape").await;
        let ddl_text = match cell.first().map(|r| r[1].clone()) {
            Some(MVal::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
            v => panic!("node {i}: non-bytes create-table cell {v:?}"),
        };
        assert!(ddl_text.contains("PRIMARY KEY") && ddl_text.contains("ENGINE=InnoDB"));
        renders.push(ddl_text);
    }
    assert!(
        renders.windows(2).all(|w| w[0] == w[1]),
        "SHOW CREATE TABLE must render identically on every node: {renders:?}"
    );

    // ---- DDL on a follower rejects with the leader error (clients
    // retry on the leader -- the RESP control-plane convention).
    for (i, conn) in conns.iter_mut().enumerate().skip(1) {
        for sql in ["TRUNCATE TABLE ctr2", "RENAME TABLE ctr2 TO elsewhere"] {
            let err = conn.query_drop(sql).await.unwrap_err().to_string();
            assert!(
                err.contains("requires the raft leader"),
                "node {i} ({sql}): {err}"
            );
        }
    }

    for mut n in nodes {
        n.kill_now();
    }
}
