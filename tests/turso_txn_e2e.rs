//! T5 turso-engine transactions, prepared statements and session edge
//! cases over one real rdb process (see plans/2026-11-09-turso-engine):
//! string-passthrough BEGIN/COMMIT/ROLLBACK on the pinned per-session
//! connection with MySQL's implicit-commit-on-BEGIN semantics, no-op
//! COMMIT/ROLLBACK outside a transaction, prepared INSERT/SELECT with
//! `?` binding, USE rejection inside an open transaction (1235), the
//! two-connection dirty-read check, and the isolation-level echo
//! (session-record only -- the engine ignores it; ledger).

mod common;

use mysql_async::prelude::Queryable;

use common::mysql::{rows, s, spawn_node_mysql_yaml, ER_NOT_SUPPORTED_YET, PASS};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready};

#[tokio::test]
async fn turso_txn_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-txn-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let turso_dir = dir.join("tursodb");
    std::fs::create_dir_all(&turso_dir).unwrap();
    let extra = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let mut node = spawn_node_mysql_yaml(&dir, 0, true, None, &extra);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    common::mysql::ddl(&mut c, "CREATE DATABASE turso_txn").await;
    c.query_drop("USE turso_txn").await.expect("use turso_txn");
    c.query_drop("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(32))")
        .await
        .expect("create t");

    // (t1) BEGIN -> INSERT -> ROLLBACK: nothing persists.
    c.query_drop("BEGIN").await.expect("begin");
    c.query_drop("INSERT INTO t VALUES (1, 'rolled')")
        .await
        .expect("insert in txn");
    c.query_drop("ROLLBACK").await.expect("rollback");
    assert_eq!(
        rows(&mut c, "SELECT COUNT(*) FROM t").await,
        vec![vec![s("0")]],
        "t1: rollback discarded the insert"
    );

    // (t2) BEGIN -> INSERT x2 -> COMMIT: both rows persist.
    c.query_drop("BEGIN").await.expect("begin");
    c.query_drop("INSERT INTO t VALUES (1, 'one')")
        .await
        .expect("insert one");
    c.query_drop("INSERT INTO t VALUES (2, 'two')")
        .await
        .expect("insert two");
    c.query_drop("COMMIT").await.expect("commit");
    assert_eq!(
        rows(&mut c, "SELECT id, v FROM t ORDER BY id").await,
        vec![vec![s("1"), s("one")], vec![s("2"), s("two")]],
        "t2: commit kept both rows"
    );

    // (t3) MySQL implicit-commit semantics: the second BEGIN commits
    // the first transaction, so the later ROLLBACK only kills the
    // second (empty) one -- the first insert SURVIVES.
    c.query_drop("BEGIN").await.expect("begin 1");
    c.query_drop("INSERT INTO t VALUES (3, 'kept')")
        .await
        .expect("insert in first txn");
    c.query_drop("BEGIN").await.expect("begin 2 (implicit commit)");
    c.query_drop("ROLLBACK").await.expect("rollback second txn");
    assert_eq!(
        common::mysql::col(&mut c, "SELECT id FROM t WHERE id = 3").await,
        vec![s("3")],
        "t3: implicit commit preserved the first insert"
    );

    // (t4) COMMIT/ROLLBACK outside any transaction are no-op OKs
    // (MySQL shape), never errors.
    c.query_drop("COMMIT").await.expect("bare commit is a no-op");
    c.query_drop("ROLLBACK")
        .await
        .expect("bare rollback is a no-op");

    // (t5) prepared statements: INSERT with `?` placeholders executed
    // twice with different values (parse once, rebind), prepared
    // SELECT with `?` in WHERE.
    c.exec_drop(
        "INSERT INTO t (id, v) VALUES (?, ?)",
        (4i64, "prepped-a"),
    )
    .await
    .expect("prepared insert a");
    c.exec_drop(
        "INSERT INTO t (id, v) VALUES (?, ?)",
        (5i64, "prepped-b"),
    )
    .await
    .expect("prepared insert b (rebind)");
    let got: Vec<(i64, String)> = c
        .exec("SELECT id, v FROM t WHERE id >= ? ORDER BY id", (3i64,))
        .await
        .expect("prepared select with ? in WHERE");
    assert_eq!(
        got,
        vec![
            (3, "kept".to_string()),
            (4, "prepped-a".to_string()),
            (5, "prepped-b".to_string()),
        ],
        "t5: prepared insert x2 + prepared WHERE ? select"
    );

    // (t6) USE of a different turso library inside an open transaction
    // rejects with 1235; the session stays pinned (and in its txn),
    // and after ROLLBACK the switch works.
    common::mysql::ddl(&mut c, "CREATE DATABASE turso_other").await;
    c.query_drop("BEGIN").await.expect("begin");
    let e = mysql_server_error(&mut c, "USE turso_other").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    assert!(
        e.message.contains("switching database inside a turso transaction"),
        "t6 message: {e:?}"
    );
    // Still on turso_txn, still inside the transaction.
    c.query_drop("INSERT INTO t VALUES (6, 'in-txn')")
        .await
        .expect("insert after rejected use");
    c.query_drop("ROLLBACK").await.expect("rollback");
    assert!(
        common::mysql::col(&mut c, "SELECT id FROM t WHERE id = 6")
            .await
            .is_empty(),
        "t6: post-error txn still rolled back"
    );
    c.query_drop("USE turso_other")
        .await
        .expect("switch works after rollback");
    c.query_drop("USE turso_txn").await.expect("switch back");

    // (t7) two connections -- dirty-read check: B must not see A's
    // uncommitted row, then must see it after A's COMMIT (turso WAL:
    // readers serve the latest committed snapshot, and the drained
    // SELECT ends B's read transaction so the next read re-snapshots).
    let mut b = mysql_root_conn(&node, PASS).await;
    b.query_drop("USE turso_txn").await.expect("b use");
    c.query_drop("BEGIN").await.expect("a begin");
    c.query_drop("INSERT INTO t VALUES (7, 'uncommitted')")
        .await
        .expect("a insert");
    assert_eq!(
        rows(&mut b, "SELECT COUNT(*) FROM t").await,
        vec![vec![s("5")]],
        "t7: B does not see A's uncommitted row"
    );
    c.query_drop("COMMIT").await.expect("a commit");
    assert_eq!(
        rows(&mut b, "SELECT COUNT(*) FROM t").await,
        vec![vec![s("6")]],
        "t7: B sees the row after A's commit"
    );

    // (t8) isolation level: accepted and echoed back verbatim
    // (documented divergence: turso libraries ignore the setting --
    // actual behavior is SQLite connection-level locking; the echoed
    // value is a session-side record only). Host session surfaces keep
    // answering on turso sessions.
    c.query_drop("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .await
        .expect("set isolation");
    assert_eq!(
        rows(&mut c, "SELECT @@transaction_isolation").await,
        vec![vec![s("READ-COMMITTED")]],
        "t8: isolation echo"
    );
    assert_eq!(
        rows(&mut c, "SELECT DATABASE()").await,
        vec![vec![s("turso_txn")]],
        "t8: DATABASE() on a turso session"
    );
    assert_eq!(
        rows(&mut c, "SELECT @@autocommit").await,
        vec![vec![s("1")]],
        "t8: @@autocommit on a turso session"
    );

    node.kill_now();
}
