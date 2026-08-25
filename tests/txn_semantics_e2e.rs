//! Phase-1 MySQL txn semantics over a real single-node rdb process:
//! SAVEPOINT/ROLLBACK TO/RELEASE visibility, session-aware
//! `@@transaction_isolation`, locking reads (FOR UPDATE / FOR SHARE).

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

/// One fresh single-node world per test: node up + table + 3 seed rows.
async fn world(name: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-txnsem-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("test dir");
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut conn = connect(&node).await;
    ddl(
        &mut conn,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
    )
    .await;
    conn.query_drop("INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .await
        .expect("seed");
    (node, conn)
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    try_rows(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// Raw outcome: server errors (1205 latch conflicts, 1305 savepoints,
/// ...) surface here instead of panicking inside `rows`.
async fn try_rows(
    conn: &mut mysql_async::Conn,
    sql: &str,
) -> Result<Vec<Vec<MVal>>, mysql_async::Error> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await?;
    Ok(rs
        .into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect())
}

async fn one_col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    rows(conn, sql)
        .await
        .into_iter()
        .map(|r| match &r[0] {
            MVal::Bytes(b) => String::from_utf8_lossy(b).to_string(),
            other => panic!("not a string: {other:?}"),
        })
        .collect()
}
#[tokio::test]
async fn savepoint_rollback_visibility() {
    let (mut node, mut a) = world("savepoint").await;

    a.query_drop("BEGIN").await.expect("begin");
    a.query_drop("INSERT INTO t (id, v) VALUES (10, 'ten')")
        .await
        .expect("staged");
    a.query_drop("SAVEPOINT sp").await.expect("savepoint");
    a.query_drop("INSERT INTO t (id, v) VALUES (20, 'twenty')")
        .await
        .expect("staged after marker");
    a.query_drop("UPDATE t SET v = 'A!' WHERE id = 1")
        .await
        .expect("staged overwrite of pre-marker row");
    assert_eq!(
        one_col(
            &mut a,
            "SELECT v FROM t WHERE id IN (1, 10, 20) ORDER BY id"
        )
        .await,
        vec!["A!", "ten", "twenty"],
        "own staged writes visible"
    );

    a.query_drop("ROLLBACK TO SAVEPOINT sp")
        .await
        .expect("rb to");
    assert_eq!(
        one_col(
            &mut a,
            "SELECT v FROM t WHERE id IN (1, 10, 20) ORDER BY id"
        )
        .await,
        vec!["a", "ten"],
        "post-marker writes undone, pre-marker staged write kept"
    );

    // unknown savepoints are MySQL-1305-shaped errors.
    let err = a
        .query_drop("ROLLBACK TO SAVEPOINT nope")
        .await
        .expect_err("unknown savepoint");
    assert!(
        err.to_string().contains("SAVEPOINT nope does not exist"),
        "wrong error: {err}"
    );

    // the marker itself survives ROLLBACK TO (idempotent), dies on
    // RELEASE, and savepoints never outlive the txn.
    a.query_drop("ROLLBACK TO SAVEPOINT sp")
        .await
        .expect("again");
    a.query_drop("RELEASE SAVEPOINT sp").await.expect("release");
    let err = a
        .query_drop("ROLLBACK TO SAVEPOINT sp")
        .await
        .expect_err("released");
    assert!(err.to_string().contains("does not exist"), "{err}");

    a.query_drop("COMMIT").await.expect("commit");
    assert_eq!(
        one_col(
            &mut a,
            "SELECT v FROM t WHERE id IN (1, 10, 20) ORDER BY id"
        )
        .await,
        vec!["a", "ten"],
        "only pre-marker writes committed"
    );
    let err = a
        .query_drop("ROLLBACK TO SAVEPOINT sp")
        .await
        .expect_err("savepoints must not survive COMMIT");
    assert!(err.to_string().contains("does not exist"), "{err}");

    // bare SAVEPOINT with no open txn implicitly starts one (MySQL).
    a.query_drop("SAVEPOINT implicit").await.expect("implicit");
    a.query_drop("INSERT INTO t (id, v) VALUES (30, 'thirty')")
        .await
        .expect("staged in implicit txn");
    a.query_drop("ROLLBACK").await.expect("rollback");
    assert_eq!(
        one_col(&mut a, "SELECT COUNT(*) FROM t WHERE id = 30").await,
        vec!["0"],
        "implicit txn rolled back"
    );
    node.kill_now();
}

#[tokio::test]
async fn transaction_isolation_round_trip() {
    let (mut node, mut a) = world("isolation").await;

    // default: MySQL's REPEATABLE-READ (the engine's snapshot
    // isolation), on both the modern and the legacy variable.
    assert_eq!(
        one_col(&mut a, "SELECT @@transaction_isolation").await,
        vec!["REPEATABLE-READ"]
    );
    assert_eq!(
        one_col(&mut a, "SELECT @@tx_isolation").await,
        vec!["REPEATABLE-READ"]
    );

    // every accepted MySQL level round-trips per session.
    for (set, reported) in [
        ("READ UNCOMMITTED", "READ-UNCOMMITTED"),
        ("READ COMMITTED", "READ-COMMITTED"),
        ("REPEATABLE READ", "REPEATABLE-READ"),
        ("SERIALIZABLE", "SERIALIZABLE"),
    ] {
        a.query_drop(format!("SET SESSION TRANSACTION ISOLATION LEVEL {set}"))
            .await
            .unwrap_or_else(|e| panic!("set {set}: {e}"));
        assert_eq!(
            one_col(&mut a, "SELECT @@transaction_isolation").await,
            vec![reported],
            "level {set}"
        );
    }

    // session scoping: a second connection keeps the default.
    let mut b = connect(&node).await;
    assert_eq!(
        one_col(&mut b, "SELECT @@transaction_isolation").await,
        vec!["REPEATABLE-READ"]
    );

    // unknown variables still error exactly like before.
    let err = a
        .query_drop("SELECT @@no_such_var")
        .await
        .expect_err("unknown var");
    assert!(err.to_string().contains("Unknown system variable"), "{err}");
    node.kill_now();
}

#[tokio::test]
async fn for_update_conflict_between_two_sessions() {
    let (mut node, mut a) = world("conflict").await;
    let mut b = connect(&node).await;

    a.query_drop("BEGIN").await.expect("A begin");
    let got = one_col(&mut a, "SELECT v FROM t WHERE id = 1 FOR UPDATE").await;
    assert_eq!(got, vec!["a"], "A reads and locks the row");

    // B's locking read on the latched row fails fast, deterministically
    // (MySQL 1205 message shape; rdb never blocks).
    b.query_drop("BEGIN").await.expect("B begin");
    let err = b
        .query_drop("SELECT v FROM t WHERE id = 1 FOR UPDATE")
        .await
        .expect_err("latch conflict");
    let msg = err.to_string();
    assert!(
        msg.contains("Lock wait timeout exceeded"),
        "expected 1205-style error, got: {msg}"
    );
    // a different row stays lockable by B.
    assert_eq!(
        one_col(&mut b, "SELECT v FROM t WHERE id = 2 FOR UPDATE").await,
        vec!["b"]
    );

    // COMMIT releases A's latches: B's retry now succeeds.
    a.query_drop("COMMIT").await.expect("A commit");
    assert_eq!(
        one_col(&mut b, "SELECT v FROM t WHERE id = 1 FOR UPDATE").await,
        vec!["a"],
        "latch released at COMMIT"
    );
    b.query_drop("ROLLBACK").await.expect("B rollback");
    node.kill_now();
}

#[tokio::test]
async fn for_update_locks_release_on_rollback_to_savepoint() {
    let (mut node, mut a) = world("rbto-latch").await;
    let mut b = connect(&node).await;

    a.query_drop("BEGIN").await.expect("begin");
    assert_eq!(
        one_col(&mut a, "SELECT v FROM t WHERE id = 1 FOR UPDATE").await,
        vec!["a"]
    );
    a.query_drop("SAVEPOINT sp").await.expect("marker");
    assert_eq!(
        one_col(&mut a, "SELECT v FROM t WHERE id = 2 FOR UPDATE").await,
        vec!["b"]
    );

    // ROLLBACK TO keeps locks taken before the marker, releases the
    // ones taken after it (MySQL row-lock semantics).
    a.query_drop("ROLLBACK TO SAVEPOINT sp")
        .await
        .expect("rb to");
    b.query_drop("BEGIN").await.expect("B begin");
    assert_eq!(
        one_col(&mut b, "SELECT v FROM t WHERE id = 2 FOR UPDATE").await,
        vec!["b"],
        "post-savepoint latch released"
    );
    let err = b
        .query_drop("SELECT v FROM t WHERE id = 1 FOR UPDATE")
        .await
        .expect_err("pre-savepoint latch kept");
    assert!(
        err.to_string().contains("Lock wait timeout exceeded"),
        "{err}"
    );

    a.query_drop("ROLLBACK").await.expect("A rollback");
    assert_eq!(
        one_col(&mut b, "SELECT v FROM t WHERE id = 1 FOR UPDATE").await,
        vec!["a"],
        "ROLLBACK releases the rest"
    );
    b.query_drop("ROLLBACK").await.expect("B rollback");
    node.kill_now();
}

#[tokio::test]
async fn for_share_composes_and_autocommit_for_update_succeeds() {
    let (mut node, mut a) = world("share").await;
    let mut b = connect(&node).await;

    // autocommit FOR UPDATE: lock taken and released at statement end,
    // the read itself answers normally.
    assert_eq!(
        one_col(&mut a, "SELECT v FROM t WHERE id = 1 FOR UPDATE").await,
        vec!["a"]
    );

    // shared locks compose across sessions; a third session's exclusive
    // taker loses while a shared holder remains.
    a.query_drop("BEGIN").await.expect("A begin");
    b.query_drop("BEGIN").await.expect("B begin");
    let mut c = connect(&node).await;
    c.query_drop("BEGIN").await.expect("C begin");
    assert_eq!(
        one_col(&mut a, "SELECT v FROM t WHERE id = 3 FOR SHARE").await,
        vec!["c"]
    );
    assert_eq!(
        one_col(&mut b, "SELECT v FROM t WHERE id = 3 FOR SHARE").await,
        vec!["c"]
    );
    let err = try_rows(&mut c, "SELECT v FROM t WHERE id = 3 FOR UPDATE")
        .await
        .expect_err("a remaining shared holder blocks C");
    assert!(
        err.to_string().contains("Lock wait timeout exceeded"),
        "{err}"
    );

    a.query_drop("ROLLBACK").await.expect("A rollback");
    let err = try_rows(&mut c, "SELECT v FROM t WHERE id = 3 FOR UPDATE")
        .await
        .expect_err("B still holds the shared latch");
    assert!(
        err.to_string().contains("Lock wait timeout exceeded"),
        "{err}"
    );
    b.query_drop("ROLLBACK").await.expect("B rollback");
    assert_eq!(
        one_col(&mut c, "SELECT v FROM t WHERE id = 3 FOR UPDATE").await,
        vec!["c"],
        "last shared holder released"
    );
    c.query_drop("ROLLBACK").await.expect("C rollback");
    node.kill_now();
}

#[tokio::test]
async fn nowait_and_skip_locked_stay_unsupported() {
    let (mut node, mut a) = world("nowait").await;
    for sql in [
        "SELECT * FROM t WHERE id = 1 FOR UPDATE NOWAIT",
        "SELECT * FROM t WHERE id = 1 FOR SHARE SKIP LOCKED",
    ] {
        let err = a.query_drop(sql).await.expect_err("unsupported");
        let msg = err.to_string();
        assert!(
            msg.contains("not supported"),
            "wrong error for {sql}: {msg}"
        );
    }
    node.kill_now();
}
