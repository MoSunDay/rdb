//! `ddl_alter` semantics: TRUNCATE (wipe + auto-inc reset + index
//! entries + columnar purge + in-txn rejection), RENAME (catalog-only,
//! data/table-id/counter preserved) and the ALTER TABLE index forms --
//! all through the real `execute` dispatch so the session/txn
//! interplay is the production path.

use crate::sql::exec::{self, ddl, ExecOutcome, SqlSession};
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::parse_statement;
use crate::sql::storage::catalog;
use crate::state::testutil;

async fn run(shared: &crate::state::Shared, sess: &mut SqlSession, sql: &str) -> ExecOutcome {
    exec::execute(shared, sess, parse_statement(sql).unwrap())
        .await
        .unwrap()
}

async fn try_run(
    shared: &crate::state::Shared,
    sess: &mut SqlSession,
    sql: &str,
) -> Result<ExecOutcome, crate::sql::parse::error::SqlError> {
    exec::execute(shared, sess, parse_statement(sql).unwrap()).await
}

/// Row rows of a statement expected to yield rows (single-column use).
async fn scalars(
    shared: &crate::state::Shared,
    sql: &str,
) -> Vec<crate::sql::storage::schema::Value> {
    let mut sess = SqlSession::default();
    match run(shared, &mut sess, sql).await {
        ExecOutcome::Rows { rows, .. } => rows.into_iter().map(|mut r| r.remove(0)).collect(),
        other => panic!("rows expected, got {other:?}"),
    }
}

async fn one_int(shared: &crate::state::Shared, sql: &str) -> i64 {
    match scalars(shared, sql).await.first() {
        Some(crate::sql::storage::schema::Value::Int(i)) => *i,
        other => panic!("one int expected, got {other:?}"),
    }
}

/// t(id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR, n BIGINT UNIQUE).
async fn world_with_rows() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    let mut sess = SqlSession::default();
    run(
        &shared,
        &mut sess,
        "CREATE TABLE t (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL, n BIGINT NULL)",
    )
    .await;
    run(&shared, &mut sess, "CREATE UNIQUE INDEX uq_n ON t (n)").await;
    run(
        &shared,
        &mut sess,
        "INSERT INTO t (v, n) VALUES ('a', 10), ('b', 20), ('c', 30)",
    )
    .await;
    shared
}

#[tokio::test]
async fn truncate_wipes_rows_resets_auto_increment_and_index_entries() {
    let shared = world_with_rows().await;
    assert_eq!(one_int(&shared, "SELECT COUNT(*) FROM t").await, 3);
    // the unique index is live: a duplicate n is vetoed
    let mut sess = SqlSession::default();
    let err = try_run(&shared, &mut sess, "INSERT INTO t (v, n) VALUES ('d', 10)")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);

    run(&shared, &mut sess, "TRUNCATE TABLE t").await;

    assert_eq!(one_int(&shared, "SELECT COUNT(*) FROM t").await, 0);
    // AUTO_INCREMENT restarted from 1
    run(&shared, &mut sess, "INSERT INTO t (v, n) VALUES ('d', 10)").await;
    assert_eq!(one_int(&shared, "SELECT id FROM t WHERE v = 'd'").await, 1);
    // the OLD unique entry died with the wipe: n=10 was claimable
    // again, and the row that claimed it now enforces the constraint
    let err = try_run(&shared, &mut sess, "INSERT INTO t (v, n) VALUES ('f', 10)")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);
    run(&shared, &mut sess, "INSERT INTO t (v, n) VALUES ('e', 11)").await;
    // schema survived verbatim (fresh table_id, same definition)
    let s = catalog::lookup(&shared, "t").unwrap().unwrap();
    assert_eq!(s.pk, vec!["id".to_string()]);
    assert_eq!(s.indexes.len(), 1);
}

#[tokio::test]
async fn truncate_rejected_inside_txn_and_not_rollbackable_after() {
    let shared = world_with_rows().await;
    let mut sess = SqlSession::default();
    run(&shared, &mut sess, "BEGIN").await;
    run(
        &shared,
        &mut sess,
        "INSERT INTO t (v, n) VALUES ('staged', 40)",
    )
    .await;
    // in-txn TRUNCATE rejects like every DDL (decision point 5)
    let err = try_run(&shared, &mut sess, "TRUNCATE t").await.unwrap_err();
    assert_eq!(err.code, ErrorCode::TxnDdl);
    assert!(
        err.msg.contains("DDL not allowed inside a transaction"),
        "{err}"
    );
    // rollback the staged row; the pre-txn rows survive untouched
    run(&shared, &mut sess, "ROLLBACK").await;
    assert_eq!(one_int(&shared, "SELECT COUNT(*) FROM t").await, 3);
    // outside a txn: the wipe is immediate and final
    run(&shared, &mut sess, "TRUNCATE t").await;
    assert_eq!(one_int(&shared, "SELECT COUNT(*) FROM t").await, 0);
}

#[tokio::test]
async fn truncate_missing_table_errors() {
    let shared = testutil::shared_with(testutil::test_config());
    let mut sess = SqlSession::default();
    let err = try_run(&shared, &mut sess, "TRUNCATE TABLE nope")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchTable);
}

#[tokio::test]
async fn truncate_columnar_table_purges_segments_and_fresh_id() {
    let shared = testutil::shared_with(testutil::test_config());
    let mut sess = SqlSession::default();
    run(
        &shared,
        &mut sess,
        "CREATE TABLE cd (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL) ENGINE=columnar",
    )
    .await;
    let schema = catalog::lookup(&shared, "cd").unwrap().unwrap();
    let rows = vec![vec![
        crate::sql::storage::schema::Value::Int(1),
        crate::sql::storage::schema::Value::Null,
    ]];
    let meta = crate::sql::columnar::writer::commit_segment(&shared, &schema, 7, 10, &rows)
        .await
        .unwrap();
    let dir = crate::sql::columnar::writer::columnar_dir(&shared.conf);
    assert!(dir.join(&meta.file).exists());

    run(&shared, &mut sess, "TRUNCATE cd").await;

    // segments of the OLD id are gone; the table reads empty but keeps
    // its definition under a fresh id
    assert_eq!(one_int(&shared, "SELECT COUNT(*) FROM cd").await, 0);
    let after = catalog::lookup(&shared, "cd").unwrap().unwrap();
    assert!(after.engine.is_columnar());
    assert!(after.id > schema.id, "fresh table id after truncate");
    assert!(
        crate::sql::columnar::registry_of(&shared)
            .segments(schema.id)
            .is_empty(),
        "old-id segments purged"
    );
    assert!(!dir.join(&meta.file).exists(), "segment file removed");
}

#[tokio::test]
async fn rename_is_catalog_only_data_and_counter_survive() {
    let shared = world_with_rows().await;
    let before = catalog::lookup(&shared, "t").unwrap().unwrap();
    let mut sess = SqlSession::default();
    run(&shared, &mut sess, "RENAME TABLE t TO u").await;

    let after = catalog::lookup(&shared, "u").unwrap().expect("renamed");
    assert_eq!(after.id, before.id, "table_id stable: zero data copy");
    assert_eq!(after.name, "u");
    assert!(catalog::lookup(&shared, "t").unwrap().is_none());
    // data + unique index + counter all live on
    assert_eq!(one_int(&shared, "SELECT COUNT(*) FROM u").await, 3);
    let err = try_run(&shared, &mut sess, "SELECT * FROM t")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchTable);
    // auto-increment CONTINUES (rename must not reset it; ids may sit
    // above the inserted count because of batch reservation)
    run(&shared, &mut sess, "INSERT INTO u (v, n) VALUES ('x', 40)").await;
    let next_id = one_int(&shared, "SELECT id FROM u WHERE v = 'x'").await;
    assert!(next_id > 3, "counter carried over, got {next_id}");
    // unique index still enforced under the new name
    let err = try_run(&shared, &mut sess, "INSERT INTO u (v, n) VALUES ('y', 10)")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);
    // and the old name's counter key is cleared: a new `t` starts at 1
    run(
        &shared,
        &mut sess,
        "CREATE TABLE t (id BIGINT AUTO_INCREMENT PRIMARY KEY)",
    )
    .await;
    run(&shared, &mut sess, "INSERT INTO t (id) VALUES (NULL)").await;
    assert_eq!(one_int(&shared, "SELECT id FROM t").await, 1);
}

#[tokio::test]
async fn rename_rejections() {
    let shared = world_with_rows().await;
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE z (id BIGINT PRIMARY KEY)").unwrap(),
    )
    .await
    .unwrap();
    let mut sess = SqlSession::default();
    let err = try_run(&shared, &mut sess, "RENAME TABLE t TO z")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::TableExists);
    let err = try_run(&shared, &mut sess, "RENAME TABLE nope TO q")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NoSuchTable);
    // ALTER TABLE ... RENAME TO is the same statement
    run(&shared, &mut sess, "ALTER TABLE z RENAME TO zz").await;
    assert!(catalog::lookup(&shared, "zz").unwrap().is_some());
    // case-only rename of the same table is allowed
    run(&shared, &mut sess, "ALTER TABLE zz RENAME TO ZZ").await;
    assert!(catalog::lookup(&shared, "ZZ").unwrap().is_some());
}

#[tokio::test]
async fn alter_table_add_and_drop_index_reuses_index_paths() {
    let shared = testutil::shared_with(testutil::test_config());
    let mut sess = SqlSession::default();
    run(
        &shared,
        &mut sess,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, n BIGINT NULL)",
    )
    .await;
    run(
        &shared,
        &mut sess,
        "INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2)",
    )
    .await;

    run(&shared, &mut sess, "ALTER TABLE t ADD INDEX idx_v (v)").await;
    let s = catalog::lookup(&shared, "t").unwrap().unwrap();
    assert_eq!(s.indexes.len(), 1);
    assert_eq!(s.indexes[0].name, "idx_v");
    assert!(!s.indexes[0].unique);

    // ADD UNIQUE via ALTER: existing duplicates veto the build
    run(&shared, &mut sess, "INSERT INTO t VALUES (3, 'a', 3)").await;
    let err = try_run(&shared, &mut sess, "ALTER TABLE t ADD UNIQUE KEY uq_v (v)")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);

    run(
        &shared,
        &mut sess,
        "ALTER TABLE t ADD UNIQUE INDEX uq_n (n)",
    )
    .await;
    let s = catalog::lookup(&shared, "t").unwrap().unwrap();
    assert_eq!(s.indexes.len(), 2);
    assert!(s.indexes[1].unique);
    let err = try_run(&shared, &mut sess, "INSERT INTO t VALUES (4, 'd', 1)")
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::DupEntry);

    // DROP INDEX via ALTER removes the catalog entry and the entries
    run(&shared, &mut sess, "ALTER TABLE t DROP INDEX uq_n").await;
    let s = catalog::lookup(&shared, "t").unwrap().unwrap();
    assert_eq!(s.indexes.len(), 1);
    run(&shared, &mut sess, "INSERT INTO t VALUES (4, 'd', 1)").await;
}
