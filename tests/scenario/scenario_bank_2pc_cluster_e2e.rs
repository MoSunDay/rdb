//! Multi-store scenario #21 -- inter-bank transfers over a THREE-node
//! SQL cluster (MySQL wire + sql_rpc 2PC + raft control plane each):
//! the opening-balance batch spans all slot bands and commits through
//! Prepare/Decide on every owner; the transfer log rides the same 2PC
//! path; a REPLAYED transfer id hits the remote unique-veto (`dup:`
//! surfacing as 1062) and vanishes on every node at once; every live
//! node reads back the SAME balances and log (M3 scatter-gather), and
//! /sql2pc/status answers `unknown` for fresh ids and `committed` for
//! the settled batch. Three REAL processes.

use crate::common;
use crate::scenario_common;
use std::time::{Duration, Instant};

use common::{cmd_one_shot, start_sql_cluster, ProcNode, TOKEN};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";
/// Accounts in the spanning batches (ids over 3 slot bands).
const ACCOUNTS: i64 = 24;

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

/// First cell of every row as i64 (bytes or int wire cells).
async fn ints(conn: &mut mysql_async::Conn, sql: &str) -> Vec<i64> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| match r.get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
            Some(MVal::Int(i)) => i,
            v => panic!("non-int cell {v:?} for {sql}"),
        })
        .collect()
}

/// Error text of one statement; "" on success.
async fn err_of(conn: &mut mysql_async::Conn, sql: &str) -> String {
    match conn.query_drop(sql).await {
        Ok(()) => String::new(),
        Err(e) => e.to_string(),
    }
}

/// Minimal HTTP/1.1 GET; full response text.
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

/// Poll SHOW TABLES until the raft-replicated catalog reaches the node.
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

#[tokio::test]
async fn bank_transfers_spanning_commit_veto_and_consistent_reads() {
    let _serial = scenario_common::serial_guard().await;
    let dir = std::env::temp_dir().join(format!("rdb-scenario-bank21-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    // a) The bank's books: accounts plus a transfer log whose txid is
    //    UNIQUE (a replayed wire must never double-post).
    ddl(
        &mut c0,
        "CREATE TABLE accounts (id BIGINT PRIMARY KEY, holder VARCHAR(24), balance INT)",
    )
    .await;
    ddl(
        &mut c0,
        "CREATE TABLE ledger (id BIGINT PRIMARY KEY, txid VARCHAR(16) NOT NULL, \
         amount INT, UNIQUE KEY uk_txid (txid))",
    )
    .await;
    wait_table(&mut c1, "accounts", &nodes[1]).await;
    wait_table(&mut c2, "accounts", &nodes[2]).await;
    wait_table(&mut c1, "ledger", &nodes[1]).await;
    wait_table(&mut c2, "ledger", &nodes[2]).await;

    // b) Opening balances: one spanning INSERT commits through 2PC on
    //    every slot owner (24 accounts, 1000 each).
    let rows: Vec<String> = (0..ACCOUNTS)
        .map(|i| format!("({}, 'h{}', 1000)", 1000 + i, i))
        .collect();
    c0.query_drop(format!(
        "INSERT INTO accounts (id, holder, balance) VALUES {}",
        rows.join(", ")
    ))
    .await
    .expect("opening balances commit");
    for (n, c) in [(0, &mut c0), (1, &mut c1), (2, &mut c2)] {
        assert_eq!(
            ints(c, "SELECT COUNT(*) FROM accounts").await,
            vec![ACCOUNTS],
            "node{n} sees all accounts"
        );
        assert_eq!(
            ints(c, "SELECT SUM(balance) FROM accounts").await,
            vec![ACCOUNTS * 1000],
            "node{n} sums the whole book"
        );
    }

    // c) Three transfers post through the same 2PC path.
    c0.query_drop(
        "INSERT INTO ledger (id, txid, amount) VALUES \
         (1, 'T-0001', 250), (2, 'T-0002', 80), (3, 'T-0003', 500)",
    )
    .await
    .expect("transfer batch commits");
    for (n, c) in [(0, &mut c0), (1, &mut c1), (2, &mut c2)] {
        assert_eq!(
            ints(c, "SELECT SUM(amount) FROM ledger").await,
            vec![830],
            "node{n} sees the posted transfers"
        );
    }

    // d) The replayed wire: a fresh row carrying txid T-0001 is vetoed
    //    (remote unique-owner reason surfaces as dup:/Duplicate), and
    //    the whole statement disappears on every node at once.
    let err = err_of(
        &mut c0,
        "INSERT INTO ledger (id, txid, amount) VALUES (9, 'T-0001', 999)",
    )
    .await;
    assert!(!err.is_empty(), "replayed wire must fail");
    assert!(
        err.contains("dup:") || err.contains("Duplicate"),
        "unexpected error: {err}"
    );
    for (n, c) in [(0, &mut c0), (1, &mut c1), (2, &mut c2)] {
        assert_eq!(
            ints(c, "SELECT COUNT(*) FROM ledger").await,
            vec![3],
            "node{n}: replay vanished everywhere"
        );
        assert_eq!(
            ints(c, "SELECT COUNT(*) FROM ledger WHERE txid = 'T-0001'").await,
            vec![1],
            "node{n}: original wire intact"
        );
    }

    // e) The settlement desk: /sql2pc/status knows nothing about a
    //    fresh id but vouches committed outcomes for the settled batch.
    let coord = &nodes[0];
    let unknown = http_get(
        &coord.http,
        &format!("/sql2pc/status?id=ts424242&node=x&raft-token={TOKEN}"),
    )
    .await;
    assert!(unknown.ends_with("unknown\n"), "{unknown}");
    let cursor_raw = cmd_one_shot(&coord.resp, TOKEN, &[b"raft", b"get", b"sql_ts_cursor"]).await;
    let cursor: usize = String::from_utf8_lossy(&cursor_raw)
        .split(|ch: char| !ch.is_ascii_digit())
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
    assert!(committed >= 1, "no committed outcome under ts{cursor}");

    for mut n in nodes {
        n.kill_now();
    }
}
