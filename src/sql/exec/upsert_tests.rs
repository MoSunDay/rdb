//! ODKU / REPLACE unit coverage (M2): affected-rows matrix, VALUES()
//! references, pk- vs unique-conflict, pk-moving updates, REPLACE over
//! multiple unique hits, txn rollback, columnar and cluster vetoes.

use super::*;
use crate::sql::exec::write;
use crate::sql::exec::{ddl, select, ExecOutcome, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::{error::ErrorCode, parse_statement};
use crate::sql::storage::schema::Value;
use crate::sql::tx;
use crate::state::testutil;

/// `u(id BIGINT PK, v VARCHAR NULL, w VARCHAR NULL)` with UNIQUE(v)
/// and UNIQUE(w): pk-only cases simply never collide on v/w.
async fn setup() -> Shared {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement(
            "CREATE TABLE u (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, w VARCHAR(64) NULL)",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    for idx in [
        "CREATE UNIQUE INDEX uv ON u (v)",
        "CREATE UNIQUE INDEX uw ON u (w)",
    ] {
        ddl::run(&shared, parse_statement(idx).unwrap())
            .await
            .unwrap();
    }
    shared
}

type R = crate::sql::parse::error::SqlResult<ExecOutcome>;

async fn insert(shared: &Shared, sess: &mut SqlSession, sql: &str) -> R {
    write::insert(shared, sess, parse_statement(sql).unwrap()).await
}

async fn go(shared: &Shared, sql: &str) -> ExecOutcome {
    insert(shared, &mut SqlSession::default(), sql)
        .await
        .unwrap()
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
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

#[tokio::test]
async fn odku_affected_rows_matrix() {
    let shared = setup().await;
    // fresh insert -> 1
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v) VALUES (1, 'a') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(1)
    );
    // pk conflict, value changes -> 2
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v) VALUES (1, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(2)
    );
    assert_eq!(
        rows(&shared, "SELECT v FROM u WHERE id = 1").await,
        vec![vec![s("b")]]
    );
    // pk conflict, identical after update -> 0 and NO extra version
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v) VALUES (1, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(0)
    );
    // mixed multi-row batch: row 2 inserts (1), row 1 updates (2)
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v) VALUES (1, 'c'), (2, 'x') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(3)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![vec![Value::Int(1), s("c")], vec![Value::Int(2), s("x")],]
    );
}

#[tokio::test]
async fn odku_values_references_and_arith() {
    let shared = setup().await;
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE c (id BIGINT PRIMARY KEY, n BIGINT NULL)").unwrap(),
    )
    .await
    .unwrap();
    go(&shared, "INSERT INTO c (id, n) VALUES (1, 10)").await;
    // plain column ref reads the EXISTING row (n = 10 + 1)
    go(
        &shared,
        "INSERT INTO c (id, n) VALUES (1, 99) ON DUPLICATE KEY UPDATE n = n + 1",
    )
    .await;
    assert_eq!(
        rows(&shared, "SELECT n FROM c WHERE id = 1").await,
        vec![vec![Value::Int(11)]]
    );
    // VALUES(col) reads the INCOMING row (n = 99 + 5)
    go(
        &shared,
        "INSERT INTO c (id, n) VALUES (1, 99) ON DUPLICATE KEY UPDATE n = VALUES(n) + 5",
    )
    .await;
    assert_eq!(
        rows(&shared, "SELECT n FROM c WHERE id = 1").await,
        vec![vec![Value::Int(104)]]
    );
}

#[tokio::test]
async fn odku_unique_index_conflict_takes_update_branch() {
    let shared = setup().await;
    go(
        &shared,
        "INSERT INTO u (id, v, w) VALUES (1, 'a', 'p'), (2, 'b', 'q')",
    )
    .await;
    // unique(v='a') conflicts with row 1 although pk 3 is fresh: the
    // UPDATE branch applies to row 1, the incoming row never lands.
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v, w) VALUES (3, 'a', 'z') ON DUPLICATE KEY UPDATE v = 'c'"
        )
        .await,
        ExecOutcome::Affected(2)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v, w FROM u ORDER BY id").await,
        vec![
            vec![Value::Int(1), s("c"), s("p")],
            vec![Value::Int(2), s("b"), s("q")],
        ]
    );
    // the vacated unique value is reusable by another pk: the old
    // entry moved with the update, no stale duplicate rejects it
    go(&shared, "INSERT INTO u (id, v, w) VALUES (9, 'a', NULL)").await;
    assert_eq!(
        rows(&shared, "SELECT id FROM u WHERE v = 'a'").await,
        vec![vec![Value::Int(9)]]
    );
}

#[tokio::test]
async fn odku_pk_change_migrates_row_and_index_entries() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v, w) VALUES (1, 'x', 'p')").await;
    // the UPDATE branch moves the pk itself: row 1 becomes row 2 and
    // every index entry moves atomically in the same write.
    assert_eq!(
        go(&shared, "INSERT INTO u (id, v, w) VALUES (1, 'y', 'q') ON DUPLICATE KEY UPDATE id = 2, v = VALUES(v)").await,
        ExecOutcome::Affected(2)
    );
    // w keeps the EXISTING row's value (unassigned columns are not
    // taken from the incoming row -- MySQL ODKU semantics)
    assert_eq!(
        rows(&shared, "SELECT id, v, w FROM u").await,
        vec![vec![Value::Int(2), s("y"), s("p")],]
    );
    // old pk gone (no dup row); both old unique values (v='x', and
    // v='y' now owned by pk 2) are consistent -- a fresh row may claim
    // the vacated 'x'
    go(&shared, "INSERT INTO u (id, v, w) VALUES (1, 'x', 'r')").await;
    assert_eq!(
        rows(&shared, "SELECT id FROM u ORDER BY id").await,
        vec![vec![Value::Int(1)], vec![Value::Int(2)],]
    );
}

#[tokio::test]
async fn replace_deletes_every_conflict_and_counts() {
    let shared = setup().await;
    go(
        &shared,
        "INSERT INTO u (id, v, w) VALUES (1, 'a', 'p'), (2, 'b', 'q')",
    )
    .await;
    // one pk hit + one hit per unique index: 2 deleted + 1 inserted
    assert_eq!(
        go(&shared, "REPLACE INTO u (id, v, w) VALUES (3, 'a', 'q')").await,
        ExecOutcome::Affected(3)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v, w FROM u").await,
        vec![vec![Value::Int(3), s("a"), s("q")],]
    );
    // plain replace of one row by pk: 1 delete + 1 insert = 2
    assert_eq!(
        go(&shared, "REPLACE INTO u (id, v, w) VALUES (3, 'z', 'z')").await,
        ExecOutcome::Affected(2)
    );
    // no conflict at all: 1
    assert_eq!(
        go(&shared, "REPLACE INTO u (id, v, w) VALUES (4, 'k', 'k')").await,
        ExecOutcome::Affected(1)
    );
    assert_eq!(
        rows(&shared, "SELECT id FROM u ORDER BY id").await,
        vec![vec![Value::Int(3)], vec![Value::Int(4)],]
    );
}

#[tokio::test]
async fn replace_then_unique_values_are_reusable() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v, w) VALUES (1, 'a', NULL)").await;
    go(&shared, "REPLACE INTO u (id, v, w) VALUES (2, 'b', NULL)").await;
    // row 1 was NOT deleted (no conflict), 'a' still owned by 1
    assert_eq!(
        rows(&shared, "SELECT id FROM u WHERE v = 'a'").await,
        vec![vec![Value::Int(1)]]
    );
    // REPLACE (5, 'a') itself conflicts on unique(v): it deletes row 1
    // and claims 'a' -- the incoming row wins, affected 2
    assert_eq!(
        go(&shared, "REPLACE INTO u (id, v, w) VALUES (5, 'a', NULL)").await,
        ExecOutcome::Affected(2)
    );
    let err = go_err(&shared, "INSERT INTO u (id, v, w) VALUES (6, 'a', NULL)").await;
    assert_eq!(err.code, ErrorCode::DupEntry, "{err}");
}

#[tokio::test]
async fn odku_freed_unique_value_is_reusable_within_one_statement() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v) VALUES (1, 'x')").await;
    // row (2,'y') inserts; row (1,'x2') pk-conflicts and MOVES row 1
    // off 'x'; row (3,'x') must then see 'x' as FREE and take the
    // INSERT branch (a stale unique snapshot would route it into the
    // UPDATE branch of the now-innocent row 1: affected 5, one row
    // short).
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v) VALUES (2, 'y'), (1, 'x2'), (3, 'x') \
             ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(4)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![
            vec![Value::Int(1), s("x2")],
            vec![Value::Int(2), s("y")],
            vec![Value::Int(3), s("x")],
        ]
    );
}

#[tokio::test]
async fn replace_freed_unique_value_spares_the_innocent_owner() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v) VALUES (1, 'x')").await;
    // (1,'y') replaces row 1 (affected 2) and frees 'x'; (2,'x') then
    // conflicts on NOTHING -- a stale snapshot would still blame row 1,
    // delete it (affected 4) and silently lose it.
    assert_eq!(
        go(&shared, "REPLACE INTO u (id, v) VALUES (1, 'y'), (2, 'x')").await,
        ExecOutcome::Affected(3)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![vec![Value::Int(1), s("y")], vec![Value::Int(2), s("x")],]
    );
}

#[tokio::test]
async fn odku_pk_move_onto_live_row_is_1062() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v) VALUES (1, 'a'), (2, 'b')").await;
    // unique(v='b') conflicts with row 2, but the assignment moves the
    // pk onto live row 1: MySQL raises ER 1062 instead of silently
    // overwriting row 1 -- and nothing of the statement lands.
    let err = go_err(
        &shared,
        "INSERT INTO u (id, v) VALUES (3, 'b') ON DUPLICATE KEY UPDATE id = 1",
    )
    .await;
    assert_eq!(err.code, ErrorCode::DupEntry, "{err}");
    assert!(
        err.msg.contains("Duplicate entry 1 for key 'PRIMARY'"),
        "{err}"
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![vec![Value::Int(1), s("a")], vec![Value::Int(2), s("b")],]
    );
}

#[tokio::test]
async fn odku_unique_value_moves_between_rows_of_one_statement() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v) VALUES (1, 'a'), (2, 'b')").await;
    // (1,'c') frees 'a'; (2,'a') claims it for row 2; (4,'a') must
    // then conflict with row 2 (NOT the stale row 1) and update it to
    // identical values: affected 2 + 2 + 0, two rows, 'a' on row 2.
    assert_eq!(
        go(
            &shared,
            "INSERT INTO u (id, v) VALUES (1, 'c'), (2, 'a'), (4, 'a') \
             ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        ExecOutcome::Affected(4)
    );
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![vec![Value::Int(1), s("c")], vec![Value::Int(2), s("a")],]
    );
}

async fn go_err(shared: &Shared, sql: &str) -> crate::sql::parse::error::SqlError {
    insert(shared, &mut SqlSession::default(), sql)
        .await
        .unwrap_err()
}

#[tokio::test]
async fn odku_updates_rollback_in_txn() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v, w) VALUES (1, 'a', 'p')").await;
    let mut sess = SqlSession::default();
    shared.sql_ts.sync_cursor_frontier();
    sess.txn = Some(tx::begin(&shared.sql_ts));
    // REPLACE and ODKU both stage into the txn buffer
    insert(
        &shared,
        &mut sess,
        "INSERT INTO u (id, v, w) VALUES (1, 'a', 'p') ON DUPLICATE KEY UPDATE v = 'z'",
    )
    .await
    .unwrap();
    insert(
        &shared,
        &mut sess,
        "REPLACE INTO u (id, v, w) VALUES (2, 'b', 'q')",
    )
    .await
    .unwrap();
    let txn = sess.txn.take().unwrap();
    tx::rollback(&shared.sql_ts, txn);
    // pre-txn truth survives both statements
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u").await,
        vec![vec![Value::Int(1), s("a")],]
    );
}

#[tokio::test]
async fn auto_increment_burns_on_odku_update_branch() {
    let shared = testutil::shared_with(testutil::test_config());
    let ai = "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(8) NULL)";
    let uv = "CREATE UNIQUE INDEX aiv ON ai (v)";
    ddl::run(&shared, parse_statement(ai).unwrap())
        .await
        .unwrap();
    ddl::run(&shared, parse_statement(uv).unwrap())
        .await
        .unwrap();
    let mut sess = SqlSession::default();
    insert(&shared, &mut sess, "INSERT INTO ai (id, v) VALUES (1, 'a')")
        .await
        .unwrap();
    // NULL id allocates (burns) 2, then the unique(v) conflict takes
    // the UPDATE branch and the allocated id is discarded -- MySQL's
    // ODKU gap behavior.
    insert(
        &shared,
        &mut sess,
        "INSERT INTO ai (v) VALUES ('a') ON DUPLICATE KEY UPDATE v = 'b'",
    )
    .await
    .unwrap();
    assert_eq!(
        sess.last_insert_id, 2,
        "the burned id was handed to the session"
    );
    insert(&shared, &mut sess, "INSERT INTO ai (v) VALUES ('c')")
        .await
        .unwrap();
    // the burned id (and its reserve-batch tail, see exec/sequence)
    // never comes back: the next allocation is strictly above 2
    let got = rows(&shared, "SELECT id, v FROM ai ORDER BY id").await;
    assert_eq!(got[0], vec![Value::Int(1), s("b")]);
    assert_eq!(got[1][1], s("c"));
    assert!(
        matches!(got[1][0], Value::Int(n) if n > 2),
        "id 2 burned: {got:?}"
    );
}

#[tokio::test]
async fn columnar_rejects_odku_and_replace() {
    let shared = testutil::shared_with(testutil::test_config());
    let cc = "CREATE TABLE cc (id BIGINT PRIMARY KEY, v VARCHAR(8) NULL) ENGINE=columnar";
    ddl::run(&shared, parse_statement(cc).unwrap())
        .await
        .unwrap();
    for sql in [
        "INSERT INTO cc (id, v) VALUES (1, 'a') ON DUPLICATE KEY UPDATE v = VALUES(v)",
        "REPLACE INTO cc (id, v) VALUES (1, 'a')",
    ] {
        let err = go_err(&shared, sql).await;
        assert_eq!(err.code, ErrorCode::NotSupported, "{sql}: {err}");
        assert!(err.msg.contains("columnar"), "{sql}: {err}");
    }
}

#[tokio::test]
async fn cluster_mode_rejects_odku_and_replace() {
    let shared = setup().await;
    // a ready cluster with any remote participant (the exact condition
    // under which a write becomes a 2PC) vetoes the conflict paths.
    *shared.topology.write().unwrap() = crate::topology::Topology {
        cluster_ready: true,
        stable_addrs: vec![shared.conf.bind.clone(), "10.9.9.9:6379".to_string()],
        per_node_slots: 8192,
        owner_map: Default::default(),
    };
    for sql in [
        "INSERT INTO u (id, v) VALUES (1, 'a') ON DUPLICATE KEY UPDATE v = VALUES(v)",
        "REPLACE INTO u (id, v) VALUES (1, 'a')",
    ] {
        let err = go_err(&shared, sql).await;
        assert_eq!(err.code, ErrorCode::NotSupported, "{sql}: {err}");
        assert!(
            err.msg.contains("not supported in cluster mode"),
            "{sql}: {err}"
        );
    }
    // back on a single-node topology the plain INSERT path is intact
    *shared.topology.write().unwrap() = crate::topology::empty();
    assert_eq!(
        go(&shared, "INSERT INTO u (id, v, w) VALUES (1, 'a', NULL)").await,
        ExecOutcome::Affected(1)
    );
}

#[tokio::test]
async fn odku_unknown_assignment_column_is_1054() {
    let shared = setup().await;
    let err = go_err(
        &shared,
        "INSERT INTO u (id) VALUES (1) ON DUPLICATE KEY UPDATE nope = 1",
    )
    .await;
    assert_eq!(err.code, ErrorCode::BadField, "{err}");
    assert!(err.msg.contains("unknown column"), "{err}");
}

#[tokio::test]
async fn odku_unique_preemption_is_1062_same_statement() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v) VALUES (1, 'a'), (2, 'b')").await;
    // row 1's update wants v='b', still owned by the DIFFERENT live
    // row 2: the batch `vacated` set must not whitewash the takeover
    // (row 2's own update later frees 'b') -- the first decided write
    // already fails, nothing of the statement lands.
    let err = go_err(
        &shared,
        "INSERT INTO u (id, v) VALUES (1, 'b'), (2, 'c') \
         ON DUPLICATE KEY UPDATE v = VALUES(v)",
    )
    .await;
    assert_eq!(err.code, ErrorCode::DupEntry, "{err}");
    assert!(err.msg.contains("Duplicate entry"), "{err}");
    assert!(err.msg.contains("'uv'"), "{err}");
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![vec![Value::Int(1), s("a")], vec![Value::Int(2), s("b")],]
    );
}

#[tokio::test]
async fn odku_unique_preemption_errors_at_statement_in_txn() {
    let shared = setup().await;
    go(&shared, "INSERT INTO u (id, v) VALUES (1, 'a'), (2, 'b')").await;
    let mut sess = SqlSession::default();
    shared.sql_ts.sync_cursor_frontier();
    sess.txn = Some(tx::begin(&shared.sql_ts));
    // MySQL errors at the STATEMENT, not at COMMIT: the update would
    // move 'b' onto row 1 while row 2 still owns it.
    let err = insert(
        &shared,
        &mut sess,
        "INSERT INTO u (id, v) VALUES (1, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v)",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry, "{err}");
    assert!(err.msg.contains("Duplicate entry"), "{err}");
    assert!(err.msg.contains("'uv'"), "{err}");
    let txn = sess.txn.take().unwrap();
    tx::rollback(&shared.sql_ts, txn);
    assert_eq!(
        rows(&shared, "SELECT id, v FROM u ORDER BY id").await,
        vec![vec![Value::Int(1), s("a")], vec![Value::Int(2), s("b")],]
    );
}
