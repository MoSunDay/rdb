//! M3 end-to-end: cross-slot distributed SQL writes via 2PC over three
//! REAL rdb processes (MySQL frontend + sql_rpc transport + raft control
//! plane each). Covered behavior:
//! - `cluster init` + `sql_nodes` registration make every node's
//!   sql_rpc port resolvable, after which a multi-row INSERT spanning
//!   slot bands commits through Prepare/Decide on every owner;
//! - rows land ONLY on their slot owner (M3 reads are node-local: the
//!   union of the three SELECTs is the table and no row duplicates);
//! - a unique value owned by another row vetoes the REMOTE prepare
//!   (the `dup:` reason rides out as MySQL error 1062) and the whole
//!   statement disappears everywhere (abort visibility);
//! - `/sql2pc/status` answers `unknown` for never-seen ids (401
//!   without the token) and `committed` for the settled txn;
//! - a dead participant makes a spanning INSERT fail without leaving
//!   partial rows on survivors; the same INSERT commits after the
//!   participant restarts.

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
/// Rows in the spanning INSERT batches (40 ids over 3 slot bands: the
/// chance they all land on one node is ~(1/3)^39).
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
            Err(mysql_async::Error::Io(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

/// DDL is leader-gated; retry until the statement sticks.
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

async fn ids(conn: &mut mysql_async::Conn, sql: &str) -> Vec<i64> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    let mut out = rs
        .into_iter()
        .map(|r| match r.get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
            v => panic!("non-bytes id cell {v:?}"),
        })
        .collect::<Vec<_>>();
    out.sort_unstable();
    out
}

/// Run one statement; "" on success, the error text otherwise.
async fn err_of(conn: &mut mysql_async::Conn, sql: &str) -> String {
    match conn.query_drop(sql).await {
        Ok(()) => String::new(),
        Err(e) => e.to_string(),
    }
}

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

/// 3-node SQL cluster: bootstrap -> joiners -> leader -> CLUSTER INIT ->
/// topology convergence -> the `sql_nodes` registry carries every bind
/// with a non-empty sql_rpc port (the precondition for 2PC routing).
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
    // Registration is a 3s ticker (leaders self-write, followers forward
    // through /sql/nodes): poll the raft-replicated registry until all
    // three nodes are present with a live sql_rpc bind.
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

/// M3 SELECTs scatter-gather: EVERY node must read back the WHOLE
/// table, exactly the expected id set from each node alike (bands are
/// disjoint and pk -> slot is pure, so nothing is missed or doubled).
fn check_gathered(per_node: &[Vec<i64>; 3], expect: &[i64], why: &str) {
    let mut want = expect.to_vec();
    want.sort_unstable();
    for (i, got) in per_node.iter().enumerate() {
        assert_eq!(got, &want, "{why}: node {i} gathered read");
    }
}

/// The id set a committed batch contributes (contiguous ranges).
fn batch_ids(lo: i64) -> Vec<i64> {
    (lo..lo + BATCH).collect()
}

/// One multi-row INSERT of BATCH rows (distinct ids and names).
fn batch_sql(lo: i64) -> String {
    let rows: Vec<String> = (0..BATCH)
        .map(|i| format!("({}, 'n{}')", lo + i, lo + i))
        .collect();
    format!("INSERT INTO items (id, name) VALUES {}", rows.join(", "))
}

/// ids per node after `SELECT id FROM items` on each live conn (M3:
/// each read is itself a cluster-wide gather, so live nodes agree).
async fn placement(cs: &mut [Option<&mut mysql_async::Conn>]) -> [Vec<i64>; 3] {
    let mut out = [Vec::new(), Vec::new(), Vec::new()];
    for (i, c) in cs.iter_mut().enumerate() {
        if let Some(conn) = c.as_deref_mut() {
            out[i] = ids(conn, "SELECT id FROM items").await;
        }
    }
    out
}

#[tokio::test]
async fn cross_slot_commit_locality_veto_and_status_route() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-2pc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR(128) NOT NULL)",
    )
    .await;
    ddl(&mut c0, "CREATE UNIQUE INDEX items_name_uq ON items (name)").await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    // ---- spanning INSERT from node0 commits through 2PC ----
    c0.query_drop(batch_sql(1)).await.expect("spanning insert");
    let got = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&got, &batch_ids(1), "2pc insert");

    // ---- unique veto from a remote owner ----
    // Insert a fresh id per node band with the already-owned name 'n1':
    // at most one band can hold both the row and the unique key (the
    // coordinator's local dup message); the others must travel the 2PC
    // veto path and surface the raw `dup:` reason.
    let mut seen_remote_veto = false;
    for band_node in 0..3 {
        let fresh = 1100 + band_node * 1100;
        let err = err_of(
            &mut c0,
            &format!("INSERT INTO items (id, name) VALUES ({fresh}, 'n1')"),
        )
        .await;
        assert!(!err.is_empty(), "dup insert must fail (band {band_node})");
        assert!(
            err.contains("dup:") || err.contains("Duplicate entry"),
            "unexpected error: {err}"
        );
        seen_remote_veto |= err.contains("dup: unique value already owned");
    }
    assert!(
        seen_remote_veto,
        "no candidate exercised the remote veto path\n{}",
        common::all_ctx(&nodes)
    );
    // Abort visibility: nothing landed anywhere, the owner is intact.
    let after = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&after, &batch_ids(1), "after dup vetoes");
    assert_eq!(
        ids(&mut c0, "SELECT id FROM items WHERE id = 1").await,
        vec![1],
        "the original owner row must survive"
    );

    // ---- /sql2pc/status on the coordinator ----
    let coord = &nodes[0];
    let unauth = http_get(
        &coord.http,
        "/sql2pc/status?id=ts1&node=x&raft-token=wrong-token",
    )
    .await;
    assert!(unauth.starts_with("HTTP/1.1 401"), "{unauth}");
    let unknown = http_get(
        &coord.http,
        &format!("/sql2pc/status?id=ts424242&node=x&raft-token={TOKEN}"),
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 200"), "{unknown}");
    assert!(unknown.ends_with("unknown\n"), "{unknown}");
    // Settled txns live anywhere below the raft ts cursor, but every
    // settled txn here spans >= 32 consecutive ts values, so a stride
    // probe of the coordinator's outcome table must find one.
    let cursor_raw = cmd_one_shot(&coord.resp, TOKEN, &[b"raft", b"get", b"sql_ts_cursor"]).await;
    let cursor: usize = String::from_utf8_lossy(&cursor_raw)
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .max()
        .unwrap_or(0);
    let mut committed = 0;
    for n in (1..=cursor.max(1)).step_by(32) {
        let body = http_get(
            &coord.http,
            &format!("/sql2pc/status?id=ts{n}&node=x&raft-token={TOKEN}"),
        )
        .await;
        if body.ends_with("committed []\n") || body.contains("\ncommitted [") {
            committed += 1;
        }
    }
    assert!(
        committed >= 1,
        "no committed outcome served under ts{cursor}"
    );

    for mut n in nodes {
        n.kill_now();
    }
}

#[tokio::test]
async fn dead_participant_aborts_and_restart_recovers() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-2pc-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut nodes = start_sql_cluster(&dir).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;
    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR(128) NOT NULL)",
    )
    .await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    c0.query_drop(batch_sql(1)).await.expect("first insert");
    let base = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&base, &batch_ids(1), "pre-crash gather");

    // ---- kill one non-coordinator node that holds rows ----
    // (40 ids over 3 slot bands: node1 holds rows with certainty in
    // practice, and the INSERT below proves it reached its owner.)
    let victim = 1;
    let survivor_conn = &mut c2;
    nodes[victim].kill_now();

    // The same spanning INSERT must fail: some rows map to the dead
    // owner, so Prepare cannot reach it -- and nothing committed.
    let err = err_of(&mut c0, &batch_sql(1001)).await;
    assert!(
        err.contains("2pc participant") || err.contains("conflict:"),
        "unexpected error for dead participant: {err}"
    );
    // M3 read contract: with a band owner down, a SELECT refuses to
    // serve partial results -- the survivor's gather fails with the
    // node-unreachable error instead of its own band alone.
    let read_err = err_of(survivor_conn, "SELECT id FROM items").await;
    assert!(
        read_err.contains("unreachable") || read_err.contains("1027"),
        "gather must fail loudly while a band owner is down: {read_err}"
    );

    // ---- restart the victim; the same INSERT now commits ----
    nodes[victim].respawn();
    wait_resp_ready(&mut nodes[victim], 30).await;
    wait_mysql_ready(&nodes[victim], 15).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let err = err_of(&mut c0, &batch_sql(1001)).await;
        if err.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "insert never succeeded after restart: {err}\n{}",
            common::all_ctx(&nodes)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // The restarted node needs a FRESH conn; the survivors keep theirs.
    let mut cv = connect(&nodes[victim]).await;
    let mut final_slots = [None, None, None];
    final_slots[0] = Some(&mut c0);
    final_slots[1] = Some(&mut c1);
    final_slots[2] = Some(&mut c2);
    final_slots[victim] = Some(&mut cv);
    let final_p = placement(&mut final_slots).await;
    let mut expect = batch_ids(1);
    expect.extend(batch_ids(1001));
    expect.sort_unstable();
    check_gathered(&final_p, &expect, "post-restart gather");

    for mut n in nodes {
        n.kill_now();
    }
}
