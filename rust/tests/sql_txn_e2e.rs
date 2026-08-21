//! Explicit BEGIN/COMMIT/ROLLBACK snapshot transactions over a real rdb
//! process: snapshot isolation between connections, repeatable reads,
//! own-write visibility, write-write conflicts (first committer wins)
//! and DDL rejection inside a transaction.

mod common;

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";

async fn connect(node: &common::ProcNode) -> mysql_async::Conn {
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => return c,
            Err(mysql_async::Error::Io(_)) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

/// DDL needs the raft leader; retry until the bootstrap node becomes one.
async fn ddl(conn: &mut mysql_async::Conn, sql: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if std::time::Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                panic!("ddl {sql}: {e}");
            }
        }
    }
}

/// One fresh single-node world per test: node up + table created.
async fn world(name: &str, table_sql: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-txn-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut conn = connect(&node).await;
    ddl(&mut conn, table_sql).await;
    (node, conn)
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect()
}

/// Text-protocol cells are always length-prefixed bytes on the wire,
/// so mysql_async hands back `Value::Bytes` for ints too. Compare raw
/// bytes, never Debug output (Debug truncates Bytes at 8 chars).
fn int(i: i64) -> MVal {
    MVal::Bytes(i.to_string().into_bytes())
}

fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}

fn one_int(rows: &[Vec<MVal>]) -> i64 {
    assert_eq!(rows.len(), 1, "expected exactly one row: {rows:?}");
    match &rows[0][0] {
        MVal::Bytes(b) => std::str::from_utf8(b).unwrap().parse().unwrap(),
        other => panic!("not bytes: {other:?}"),
    }
}

#[tokio::test]
async fn snapshot_isolation_between_connections() {
    let (mut node, mut a) = world(
        "si",
        "CREATE TABLE si (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    let mut b = connect(&node).await;

    a.query_drop("BEGIN").await.expect("begin");
    assert_eq!(one_int(&rows(&mut a, "SELECT COUNT(*) FROM si").await), 0);

    // conn B commits a row AFTER A's snapshot: A must not see it.
    b.query_drop("INSERT INTO si (id, v) VALUES (1, 'late')")
        .await
        .expect("b insert");
    assert_eq!(
        one_int(&rows(&mut a, "SELECT COUNT(*) FROM si").await),
        0,
        "A's snapshot predates B's commit"
    );
    // B sees its own committed write.
    assert_eq!(one_int(&rows(&mut b, "SELECT COUNT(*) FROM si").await), 1);

    a.query_drop("COMMIT").await.expect("commit");
    assert_eq!(
        one_int(&rows(&mut a, "SELECT COUNT(*) FROM si").await),
        1,
        "after COMMIT, A reads a fresh snapshot"
    );
    node.kill_now();
}

#[tokio::test]
async fn repeatable_read_inside_txn() {
    let (mut node, mut a) = world(
        "rr",
        "CREATE TABLE rr (id BIGINT PRIMARY KEY, score BIGINT NOT NULL)",
    )
    .await;
    let mut b = connect(&node).await;
    b.query_drop("INSERT INTO rr (id, score) VALUES (1, 10)")
        .await
        .expect("seed");

    a.query_drop("BEGIN").await.expect("begin");
    let first = rows(&mut a, "SELECT score FROM rr WHERE id = 1").await;
    assert_eq!(first, vec![vec![int(10)]]);

    // another connection bumps the score (autocommit): A must keep
    // reading the value from ITS snapshot.
    b.query_drop("UPDATE rr SET score = score + 1 WHERE id = 1")
        .await
        .expect("b update");
    let again = rows(&mut a, "SELECT score FROM rr WHERE id = 1").await;
    assert_eq!(again, vec![vec![int(10)]], "repeatable read");

    a.query_drop("COMMIT").await.expect("commit");
    let after = rows(&mut a, "SELECT score FROM rr WHERE id = 1").await;
    assert_eq!(after, vec![vec![int(11)]]);
    node.kill_now();
}

#[tokio::test]
async fn own_write_visibility_and_rollback() {
    let (mut node, mut a) = world(
        "own",
        "CREATE TABLE own (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    let mut b = connect(&node).await;

    a.query_drop("BEGIN").await.expect("begin");
    a.query_drop("INSERT INTO own (id, v) VALUES (100, 'a')")
        .await
        .expect("staged insert");
    assert_eq!(
        rows(&mut a, "SELECT v FROM own WHERE id = 100").await,
        vec![vec![s("a")]],
        "own staged insert is visible"
    );

    a.query_drop("UPDATE own SET v = 'b' WHERE id = 100")
        .await
        .expect("staged update");
    assert_eq!(
        rows(&mut a, "SELECT v FROM own WHERE id = 100").await,
        vec![vec![s("b")]],
        "own staged update replaces the staged insert"
    );

    a.query_drop("DELETE FROM own WHERE id = 100")
        .await
        .expect("staged delete");
    assert_eq!(
        one_int(&rows(&mut a, "SELECT COUNT(*) FROM own").await),
        0,
        "own staged delete hides the row"
    );

    a.query_drop("ROLLBACK").await.expect("rollback");
    assert_eq!(
        one_int(&rows(&mut a, "SELECT COUNT(*) FROM own").await),
        0,
        "rollback discards the staged insert"
    );
    assert_eq!(
        one_int(&rows(&mut b, "SELECT COUNT(*) FROM own").await),
        0,
        "nothing ever reached the store"
    );
    node.kill_now();
}

#[tokio::test]
async fn commit_persists_staged_writes() {
    let (mut node, mut a) = world(
        "p",
        "CREATE TABLE p (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    let mut b = connect(&node).await;

    a.query_drop("BEGIN").await.expect("begin");
    a.query_drop("INSERT INTO p (id, v) VALUES (7, 'seven')")
        .await
        .expect("staged insert");
    assert_eq!(
        one_int(&rows(&mut b, "SELECT COUNT(*) FROM p").await),
        0,
        "not visible before commit"
    );
    a.query_drop("COMMIT").await.expect("commit");
    assert_eq!(
        rows(&mut b, "SELECT id, v FROM p WHERE id = 7").await,
        vec![vec![int(7), s("seven")]],
        "committed writes are durable and visible to others"
    );
    node.kill_now();
}

#[tokio::test]
async fn write_write_conflict_first_committer_wins() {
    let (mut node, mut a) = world(
        "ww",
        "CREATE TABLE ww (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    let mut b = connect(&node).await;
    b.query_drop("INSERT INTO ww (id, v) VALUES (1, 'init')")
        .await
        .expect("seed");

    a.query_drop("BEGIN").await.expect("A begin");
    b.query_drop("BEGIN").await.expect("B begin");

    a.query_drop("UPDATE ww SET v = 'from-a' WHERE id = 1")
        .await
        .expect("A stages update");
    b.query_drop("UPDATE ww SET v = 'from-b' WHERE id = 1")
        .await
        .expect("B stages update");

    a.query_drop("COMMIT").await.expect("A commits first");
    // B wrote the same pk after its snapshot: COMMIT must fail.
    let err = b
        .query_drop("COMMIT")
        .await
        .expect_err("conflicting commit must fail");
    let msg = err.to_string();
    assert!(msg.contains("conflict"), "expected conflict, got: {msg}");

    // the winner's value survived; B wrote nothing.
    assert_eq!(
        rows(&mut a, "SELECT v FROM ww WHERE id = 1").await,
        vec![vec![s("from-a")]]
    );
    node.kill_now();
}

#[tokio::test]
async fn ddl_rejected_inside_txn() {
    let (mut node, mut a) = world("ddl", "CREATE TABLE base (id BIGINT PRIMARY KEY)").await;

    a.query_drop("BEGIN").await.expect("begin");
    for sql in [
        "CREATE TABLE nope (id BIGINT PRIMARY KEY)",
        "DROP TABLE base",
        "CREATE INDEX noix ON base (id)",
    ] {
        let err = a.query_drop(sql).await.expect_err("DDL must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("DDL not allowed inside a transaction"),
            "wrong error for {sql}: {msg}"
        );
    }
    // SHOW stays allowed inside the txn, and DDL works again after it.
    assert_eq!(one_int(&rows(&mut a, "SELECT COUNT(*) FROM base").await), 0);
    a.query_drop("ROLLBACK").await.expect("rollback");
    ddl(&mut a, "CREATE TABLE fine (id BIGINT PRIMARY KEY)").await;
    node.kill_now();
}
