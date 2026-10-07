//! SQL-plane leader failover e2e over three REAL rdb processes. The
//! existing cluster suites never kill the SQL leader (RESP-side
//! failover only); here the leader of a 3-node SQL cluster is
//! SIGKILLed and the survivors must:
//! - elect a new leader that accepts DDL (raft catalog) and INSERTs
//!   (rows in live slot bands commit; rows owned by the corpse fail
//!   LOUDLY -- "2pc participant"/unreachable -- never partial);
//! - keep serving timestamp blocks (`/sql/ts?n=` on the new leader,
//!   404 `not leader` on followers), so the write path does not stall;
//! - keep prepared statements on surviving connections either working
//!   or erroring cleanly (gather refuses partial results with 1027
//!   while a band owner is down) -- under a hard timeout, never hang;
//! - drop the killed leader's connection with a transport error (the
//!   client then reconnects to the new leader);
//! - after the corpse restarts on the same config + data dir: every
//!   member serves the pre-kill baseline (MVCC read consistency) and
//!   multi-band writes resume.

mod common;

use std::time::{Duration, Instant};

use common::mysql::{connect_root, ddl, rows, s, wait_table};
use common::TOKEN;
use common::{all_ctx, start_sql_cluster, wait_leader, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

/// Minimal HTTP/1.1 GET over a raw socket; status line + body.
async fn http_get(addr: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .expect("http connect");
    sock.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: rdb\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .expect("http write");
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// `rows` that maps errors to their text (poll loops retry them).
async fn try_rows(conn: &mut mysql_async::Conn, sql: &str) -> Result<Vec<Vec<MVal>>, String> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.map_err(|e| e.to_string())?;
    Ok(rs
        .into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect())
}

/// Poll `SELECT COUNT(*) FROM <table>` on `conn` until it returns
/// `want` (the restarted node's data plane catches up through raft).
async fn poll_count(conn: &mut mysql_async::Conn, table: &str, want: i64, ctx: &str) {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        match try_rows(conn, &sql).await {
            Ok(r) if r == vec![vec![s(&want.to_string())]] => return,
            Ok(other) => assert!(
                Instant::now() < deadline,
                "{sql} returned {other:?}, wanted {want}\n{ctx}"
            ),
            Err(e) => assert!(Instant::now() < deadline, "{sql} kept failing: {e}\n{ctx}"),
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn kill9_sql_leader_new_leader_serves_ddl_ts_and_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut nodes = start_sql_cluster(dir.path(), 3).await;
    let leader = wait_leader(&nodes, 60).await;

    // ---- pre-kill: catalog + replicated baseline rows ----
    let mut lc = connect_root(&nodes[leader]).await;
    ddl(
        &mut lc,
        "CREATE TABLE fail (id BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL)",
    )
    .await;
    lc.query_drop("INSERT INTO fail (id, v) VALUES (1,'pre'),(2,'pre'),(3,'pre')")
        .await
        .expect("baseline insert");
    assert_eq!(
        rows(&mut lc, "SELECT COUNT(*) FROM fail").await,
        vec![vec![s("3")]]
    );

    // A prepared statement on a SURVIVING follower (outlives the kill).
    let follower = (leader + 1) % 3;
    let mut fc = connect_root(&nodes[follower]).await;
    let stmt = fc
        .prep("SELECT COUNT(*) FROM fail WHERE id > ?")
        .await
        .expect("prep on follower");
    let got: Vec<(i64,)> = fc.exec(&stmt, (0i64,)).await.expect("exec pre-kill");
    assert_eq!(got, vec![(3,)]);

    // ---- SIGKILL the leader, no graceful shutdown ----
    nodes[leader].kill_now();

    // The killed leader's connection must ERROR cleanly, never hang.
    let stale = tokio::time::timeout(
        Duration::from_secs(10),
        lc.query::<mysql_async::Row, _>("SELECT 1"),
    )
    .await
    .expect("query on the killed leader hung");
    assert!(stale.is_err(), "the SIGKILLed leader still answered");

    // Survivors (2 of 3 = quorum) elect a new leader.
    let new = wait_leader(&nodes, 120).await;
    assert_ne!(
        new,
        leader,
        "the corpse cannot still lead\n{}",
        all_ctx(&nodes)
    );

    // ---- DDL commits on the new leader (raft-replicated catalog) ----
    let mut nc = connect_root(&nodes[new]).await;
    ddl(
        &mut nc,
        "CREATE TABLE fail_post (id BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL)",
    )
    .await;

    // ---- the ts allocator recovered: /sql/ts serves on the new
    // leader only (followers answer 404 `not leader`) ----
    let lease = http_get(&nodes[new].http, &format!("/sql/ts?n=4&raft-token={TOKEN}")).await;
    assert!(
        lease.starts_with("HTTP/1.1 200"),
        "ts lease on the new leader: {lease}"
    );
    let range: Vec<u64> = lease[lease.find("\r\n\r\n").unwrap_or(0)..]
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    assert_eq!(range.len(), 2, "lease body: {lease}");
    assert!(range[0] > 0 && range[1] > range[0], "lease body: {lease}");
    // The OTHER survivor (neither the corpse nor the new leader).
    let other = 3 - leader - new;
    let flw = http_get(
        &nodes[other].http,
        &format!("/sql/ts?n=4&raft-token={TOKEN}"),
    )
    .await;
    assert!(
        flw.starts_with("HTTP/1.1 404") && flw.contains("not leader"),
        "follower must refuse ts leases: {flw}"
    );

    // ---- writes continue: live-band rows commit, corpse-band rows
    // fail LOUDLY (no partial commit anywhere) ----
    let (mut ok, mut refused) = (0, 0);
    for id in 1..=12 {
        let sql = format!("INSERT INTO fail_post (id, v) VALUES ({id}, 'post')");
        match nc.query_drop(&sql).await {
            Ok(()) => ok += 1,
            Err(e) => {
                refused += 1;
                let msg = e.to_string();
                assert!(
                    msg.contains("2pc participant") || msg.contains("unreachable"),
                    "dead-band write must fail loudly, got: {msg}"
                );
            }
        }
    }
    assert_eq!(ok + refused, 12);
    assert!(ok >= 1, "no live-band write committed\n{}", all_ctx(&nodes));

    // ---- the surviving follower's prepared statement: works or fails
    // cleanly under a hard timeout (the gather refuses partial reads
    // while a band owner is down) ----
    let reexec = tokio::time::timeout(
        Duration::from_secs(15),
        fc.exec::<(i64,), &mysql_async::Statement, _>(&stmt, (0i64,)),
    )
    .await;
    match reexec {
        Err(_) => panic!("prepared exec on the survivor hung"),
        Ok(Err(e)) => {
            let msg = e.to_string();
            assert!(
                msg.contains("unreachable") || msg.contains("1027"),
                "expected the loud band-owner-down error, got: {msg}"
            );
        }
        Ok(Ok(v)) => assert_eq!(v, vec![(3,)]),
    }
    // The connection itself stays usable for leader-free statements.
    assert_eq!(rows(&mut fc, "SELECT 7").await, vec![vec![s("7")]]);

    for n in nodes.iter_mut() {
        n.kill_now();
    }
}

#[tokio::test]
async fn killed_leader_restarts_and_pre_kill_data_stays_visible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut nodes = start_sql_cluster(dir.path(), 3).await;
    let leader = wait_leader(&nodes, 60).await;

    // Baseline written through the leader, replicated to every member.
    let mut lc = connect_root(&nodes[leader]).await;
    ddl(
        &mut lc,
        "CREATE TABLE base (id BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL)",
    )
    .await;
    lc.query_drop(
        "INSERT INTO base (id, v) VALUES \
         (1,'pre'),(2,'pre'),(3,'pre'),(4,'pre'),(5,'pre'),(6,'pre'),\
         (7,'pre'),(8,'pre'),(9,'pre')",
    )
    .await
    .expect("baseline insert");
    assert_eq!(
        rows(&mut lc, "SELECT COUNT(*) FROM base").await,
        vec![vec![s("9")]]
    );

    // ---- SIGKILL, elect, restart the corpse on the same dir ----
    nodes[leader].kill_now();
    let new = wait_leader(&nodes, 120).await;
    assert_ne!(
        new,
        leader,
        "the corpse cannot still lead\n{}",
        all_ctx(&nodes)
    );

    let mut nc = connect_root(&nodes[new]).await;
    ddl(
        &mut nc,
        "CREATE TABLE base_post (id BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL)",
    )
    .await;

    nodes[leader].respawn();
    wait_resp_ready(&mut nodes[leader], 30).await;
    wait_mysql_ready(&nodes[leader], 15).await;
    let mut rc = connect_root(&nodes[leader]).await;
    // Catalog catch-up first (raft log replay), then the data plane.
    wait_table(&mut rc, "base", true).await;
    wait_table(&mut rc, "base_post", true).await;
    poll_count(&mut rc, "base", 9, &all_ctx(&nodes)).await;

    // ---- pre-kill data visible from EVERY member post-failover ----
    for (i, n) in nodes.iter().enumerate() {
        let mut c = connect_root(n).await;
        poll_count(&mut c, "base", 9, &format!("node {i}\n{}", all_ctx(&nodes))).await;
    }

    // ---- multi-band writes resume on the whole cluster ----
    nc.query_drop("INSERT INTO base (id, v) VALUES (101,'rejoin'),(102,'rejoin')")
        .await
        .expect("post-rejoin insert");
    for (i, n) in nodes.iter().enumerate() {
        let mut c = connect_root(n).await;
        poll_count(
            &mut c,
            "base",
            11,
            &format!("node {i}\n{}", all_ctx(&nodes)),
        )
        .await;
    }

    for n in nodes.iter_mut() {
        n.kill_now();
    }
}
