//! INSERT ... SELECT / INSERT ... SET unit coverage (M2): same-table
//! snapshot reads, shared row construction (defaults, coercion, NOT
//! NULL), conflict actions over SELECT sources, arity errors, and the
//! columnar veto.

use super::*;
use crate::sql::exec::{ddl, select, write, ExecOutcome, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::{error::ErrorCode, parse_statement};
use crate::sql::storage::schema::Value;
use crate::sql::tx;
use crate::state::testutil;

/// `t(id BIGINT PK, v VARCHAR NULL, n BIGINT NULL)`.
async fn setup() -> Shared {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement(
            "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, n BIGINT NULL)",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    shared
}

async fn insert(
    shared: &Shared,
    sess: &mut SqlSession,
    sql: &str,
) -> crate::sql::parse::error::SqlResult<ExecOutcome> {
    write::insert(shared, sess, parse_statement(sql).unwrap()).await
}

async fn go(shared: &Shared, sql: &str) -> ExecOutcome {
    insert(shared, &mut SqlSession::default(), sql)
        .await
        .unwrap()
}

async fn rows(shared: &Shared, sql: &str) -> Vec<Vec<Value>> {
    let Statement::Select(q) = parse_statement(sql).unwrap() else {
        panic!("select");
    };
    select::run(shared, &SqlSession::default(), q)
        .await
        .unwrap()
        .1
}

/// The source materializes BEFORE any write: reading the target table
/// sees the pre-statement snapshot, not its own output (no unbounded
/// growth, no half-applied rows).
#[tokio::test]
async fn same_table_reads_pre_statement_snapshot() {
    let shared = setup().await;
    go(
        &shared,
        "INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    )
    .await;
    assert_eq!(
        go(&shared, "INSERT INTO t (id, v) SELECT id + 10, v FROM t").await,
        ExecOutcome::Affected(3)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM t ORDER BY id").await,
        vec![
            vec![Value::Int(1), Value::Str("a".into())],
            vec![Value::Int(2), Value::Str("b".into())],
            vec![Value::Int(3), Value::Str("c".into())],
            vec![Value::Int(11), Value::Str("a".into())],
            vec![Value::Int(12), Value::Str("b".into())],
            vec![Value::Int(13), Value::Str("c".into())],
        ]
    );
}

/// SELECT-sourced rows flow through the SAME row construction as
/// VALUES: named column mapping, expression cells, missing columns
/// NULL, coercion to the column types.
#[tokio::test]
async fn shares_row_construction_with_values() {
    let shared = setup().await;
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE o (a BIGINT PRIMARY KEY, b VARCHAR(8) NULL)").unwrap(),
    )
    .await
    .unwrap();
    go(&shared, "INSERT INTO o (a, b) VALUES (1, 'x'), (2, 'y')").await;
    // named subset + arithmetic on the source cells; n omitted -> NULL
    assert_eq!(
        go(
            &shared,
            "INSERT INTO t (id, n) SELECT a * 100, a + 1 FROM o WHERE a = 2"
        )
        .await,
        ExecOutcome::Affected(1)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v, n FROM t").await,
        vec![vec![Value::Int(200), Value::Null, Value::Int(3)],]
    );
    // positional form must cover the whole table width
    let err = insert(
        &shared,
        &mut SqlSession::default(),
        "INSERT INTO t SELECT a, b FROM o",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongValueCount, "{err}");
}

#[tokio::test]
async fn select_source_respects_conflict_actions() {
    let shared = setup().await;
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE s (x BIGINT PRIMARY KEY, y VARCHAR(8) NULL)").unwrap(),
    )
    .await
    .unwrap();
    go(&shared, "INSERT INTO t (id, v) VALUES (1, 'old')").await;
    go(
        &shared,
        "INSERT INTO s (x, y) VALUES (1, 'new'), (9, 'fresh')",
    )
    .await;
    // ODKU over a SELECT source: row (1,*) updates, row (9,*) inserts
    assert_eq!(
        go(
            &shared,
            "INSERT INTO t (id, v) SELECT x, y FROM s ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(3)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM t ORDER BY id").await,
        vec![
            vec![Value::Int(1), Value::Str("new".into())],
            vec![Value::Int(9), Value::Str("fresh".into())],
        ]
    );
    // REPLACE over a SELECT source: the pk hit replaces (2 affected)
    assert_eq!(
        go(
            &shared,
            "REPLACE INTO t (id, v) SELECT x, CONCAT(y, '!') FROM s WHERE x = 1"
        )
        .await,
        ExecOutcome::Affected(2)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM t WHERE id = 1").await,
        vec![vec![Value::Int(1), Value::Str("new!".into())],]
    );
}

/// `INSERT ... SET` normalizes to a named single-row VALUES list: same
/// defaults, same conflict semantics as the explicit list form.
#[tokio::test]
async fn insert_set_normalizes_to_values() {
    let shared = setup().await;
    assert_eq!(
        go(&shared, "INSERT INTO t SET id = 1, v = 'a', n = 7").await,
        ExecOutcome::Affected(1)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v, n FROM t").await,
        vec![vec![Value::Int(1), Value::Str("a".into()), Value::Int(7)],]
    );
    // SET + ODKU combo behaves like the VALUES form
    assert_eq!(
        go(
            &shared,
            "INSERT INTO t SET id = 1, v = 'b' ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(2)
    );
    assert_eq!(
        rows(&shared, "SELECT v FROM t WHERE id = 1").await,
        vec![vec![Value::Str("b".into())],]
    );
}

#[tokio::test]
async fn select_source_stages_into_txn_and_rolls_back() {
    let shared = setup().await;
    go(&shared, "INSERT INTO t (id, v) VALUES (1, 'a')").await;
    let mut sess = SqlSession::default();
    shared.sql_ts.sync_cursor_frontier();
    sess.txn = Some(tx::begin(&shared.sql_ts));
    insert(
        &shared,
        &mut sess,
        "INSERT INTO t (id, v) SELECT id + 5, v FROM t",
    )
    .await
    .unwrap();
    let txn = sess.txn.take().unwrap();
    tx::rollback(&shared.sql_ts, txn);
    assert_eq!(
        rows(&shared, "SELECT id FROM t").await,
        vec![vec![Value::Int(1)]]
    );
}

#[tokio::test]
async fn columnar_target_rejects_insert_select() {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement(
            "CREATE TABLE cc (id BIGINT PRIMARY KEY, v VARCHAR(8) NULL) ENGINE=columnar",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let err = insert(
        &shared,
        &mut SqlSession::default(),
        "INSERT INTO cc (id, v) SELECT id, v FROM cc",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotSupported, "{err}");
    assert!(err.msg.contains("columnar"), "{err}");
}

/// NOT NULL columns still reject NULLs arriving through the SELECT
/// source (the shared row construction enforces them).
#[tokio::test]
async fn select_source_enforces_not_null() {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE nn (id BIGINT PRIMARY KEY, v VARCHAR(8) NOT NULL)").unwrap(),
    )
    .await
    .unwrap();
    go(&shared, "INSERT INTO nn (id, v) VALUES (1, 'x')").await;
    let err = insert(
        &shared,
        &mut SqlSession::default(),
        "INSERT INTO nn (id) SELECT id + 1 FROM nn",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadNull, "{err}");
}
