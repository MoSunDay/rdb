use super::*;
use crate::sql::exec::{ddl, select, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::parse_statement;
use crate::sql::storage::catalog;
use crate::state::testutil;

/// Fresh stub engine with table `t(id BIGINT PK, v VARCHAR NULL)`.
async fn setup() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)").unwrap(),
    )
    .await
    .unwrap();
    shared
}

async fn exec(shared: &crate::state::Shared, sql: &str) -> ExecOutcome {
    write(shared, parse_statement(sql).unwrap()).await.unwrap()
}

async fn write(shared: &crate::state::Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    let mut sess = SqlSession::default();
    write_in(shared, &mut sess, stmt).await
}

async fn write_in(
    shared: &crate::state::Shared,
    sess: &mut SqlSession,
    stmt: Statement,
) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::Insert { .. } => insert(shared, sess, stmt).await,
        Statement::Update { .. } => update(shared, sess, stmt).await,
        Statement::Delete { .. } => delete(shared, sess, stmt).await,
        other => panic!("not a write: {other:?}"),
    }
}

async fn rows(
    shared: &crate::state::Shared,
    sql: &str,
) -> (Vec<crate::sql::exec::ColMeta>, Vec<Vec<Value>>) {
    let Statement::Select(q) = parse_statement(sql).unwrap() else {
        panic!("select");
    };
    select::run(shared, &SqlSession::default(), q)
        .await
        .unwrap()
}

#[tokio::test]
async fn insert_writes_physical_versions() {
    let shared = setup().await;
    assert!(matches!(
        exec(&shared, "INSERT INTO t (id, v) VALUES (1, 'a'), (2, NULL)").await,
        ExecOutcome::Affected(2)
    ));
    // one live physical version per row, decodable back to values
    let schema = catalog::lookup(&shared, "t").unwrap().unwrap();
    for (pk, v) in [(1, Some("a")), (2, None)] {
        let key = row::pk_encode(&Value::Int(pk)).unwrap();
        let raw = newest_raw(&shared, &schema, &key).expect("version present");
        let (header, vals) = row::decode_version(&schema, &raw).unwrap();
        assert_eq!(header, row::HEADER_LIVE);
        assert_eq!(
            vals,
            vec![
                Value::Int(pk),
                v.map(|s| Value::Str(s.to_string())).unwrap_or(Value::Null)
            ]
        );
    }
}

/// Newest raw version bytes of one pk (scan descends ts via key order).
fn newest_raw(
    shared: &crate::state::Shared,
    schema: &TableSchema,
    pk_key: &[u8],
) -> Option<Vec<u8>> {
    let mut found: Option<Vec<u8>> = None;
    ops::for_each_from(&shared.store, b"0/", false, &mut |key, val| {
        if let Some((_, table_id, pk, _)) = row::parse_version_key(key) {
            if table_id == schema.id && pk == pk_key && found.is_none() {
                found = Some(val.to_vec());
            }
        }
        found.is_none()
    })
    .unwrap();
    found
}

#[tokio::test]
async fn insert_enforces_not_null_and_pk() {
    let shared = setup().await;
    // NOT NULL on the implicitly-non-null primary key
    let err = write(
        &shared,
        parse_statement("INSERT INTO t (id, v) VALUES (NULL, 'x')").unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadNull);
    // declared NOT NULL column via a second table
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE s (id BIGINT PRIMARY KEY, v VARCHAR(8) NOT NULL)").unwrap(),
    )
    .await
    .unwrap();
    let err = write(
        &shared,
        parse_statement("INSERT INTO s (id) VALUES (1)").unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadNull);
    // positional form must cover every column
    let err = write(
        &shared,
        parse_statement("INSERT INTO t VALUES (1)").unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongValueCount);
    // unknown column
    let err = write(
        &shared,
        parse_statement("INSERT INTO t (id, zz) VALUES (1, 2)").unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadField);
}

#[tokio::test]
async fn duplicate_pk_in_one_batch_last_wins() {
    let shared = setup().await;
    exec(
        &shared,
        "INSERT INTO t (id, v) VALUES (1, 'first'), (1, 'last')",
    )
    .await;
    let (_, got) = rows(&shared, "SELECT v FROM t").await;
    assert_eq!(got, vec![vec![Value::Str("last".into())]]);
}

#[tokio::test]
async fn update_assignments_and_matching() {
    let shared = setup().await;
    exec(
        &shared,
        "INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, NULL)",
    )
    .await;
    // SET from another column (typed), WHERE drops NULL v
    assert!(matches!(
        exec(&shared, "UPDATE t SET id = id + 10 WHERE v = 'b'").await,
        ExecOutcome::Affected(1)
    ));
    let (_, got) = rows(&shared, "SELECT id, v FROM t ORDER BY id").await;
    // ids now 1, 3, 4, 12
    assert_eq!(got[3], vec![Value::Int(12), Value::Str("b".into())]);
    // ORDER BY + LIMIT picks exactly one row (highest id: 12)
    assert!(matches!(
        exec(&shared, "UPDATE t SET v = 'z' ORDER BY id DESC LIMIT 1").await,
        ExecOutcome::Affected(1)
    ));
    let (_, got) = rows(&shared, "SELECT id, v FROM t WHERE v = 'z'").await;
    assert_eq!(got, vec![vec![Value::Int(12), Value::Str("z".into())]]);
    // no-op assignment writes no version
    let before = count_versions(&shared);
    assert!(matches!(
        exec(&shared, "UPDATE t SET v = 'z' WHERE id = 12").await,
        ExecOutcome::Affected(0)
    ));
    assert_eq!(count_versions(&shared), before);
}

fn count_versions(shared: &crate::state::Shared) -> usize {
    let mut n = 0;
    ops::for_each_from(&shared.store, b"0/", false, &mut |key, _| {
        if row::parse_version_key(key).is_some() {
            n += 1;
        }
        true
    })
    .unwrap();
    n
}

#[tokio::test]
async fn update_pk_change_tombstones_old_key() {
    let shared = setup().await;
    exec(&shared, "INSERT INTO t (id, v) VALUES (1, 'a')").await;
    assert!(matches!(
        exec(&shared, "UPDATE t SET id = 9 WHERE id = 1").await,
        ExecOutcome::Affected(1)
    ));
    let schema = catalog::lookup(&shared, "t").unwrap().unwrap();
    // old pk: newest version is a tombstone; new pk: live
    let old = row::pk_encode(&Value::Int(1)).unwrap();
    let raw = newest_raw(&shared, &schema, &old).unwrap();
    assert_eq!(
        row::decode_version(&schema, &raw).unwrap().0,
        row::HEADER_TOMBSTONE
    );
    let (_, got) = rows(&shared, "SELECT id, v FROM t").await;
    assert_eq!(got, vec![vec![Value::Int(9), Value::Str("a".into())]]);
}

#[tokio::test]
async fn older_read_ts_sees_older_version() {
    let shared = setup().await;
    exec(&shared, "INSERT INTO t (id, v) VALUES (1, 'old')").await;
    let then = shared.sql_ts.now();
    exec(&shared, "UPDATE t SET v = 'new' WHERE id = 1").await;
    // physical check: two versions, newest-first in key order
    let schema = catalog::lookup(&shared, "t").unwrap().unwrap();
    let key = row::pk_encode(&Value::Int(1)).unwrap();
    let raw = newest_raw(&shared, &schema, &key).unwrap();
    let (_, vals) = row::decode_version(&schema, &raw).unwrap();
    assert_eq!(vals[1], Value::Str("new".into()));
    // snapshot scan at the older ts still sees 'old'
    let src = scan::visible_rows(&shared.store, &schema, then).unwrap();
    assert_eq!(src[0][1], Value::Str("old".into()));
}

#[tokio::test]
async fn delete_honors_filter_and_limit() {
    let shared = setup().await;
    exec(
        &shared,
        "INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    )
    .await;
    assert!(matches!(
        exec(&shared, "DELETE FROM t WHERE id IN (1, 2)").await,
        ExecOutcome::Affected(2)
    ));
    assert!(matches!(
        exec(&shared, "DELETE FROM t ORDER BY id DESC LIMIT 1").await,
        ExecOutcome::Affected(1)
    ));
    let (_, got) = rows(&shared, "SELECT id FROM t").await;
    assert_eq!(got, Vec::<Vec<Value>>::new());
    assert!(matches!(
        exec(&shared, "DELETE FROM t").await,
        ExecOutcome::Affected(0)
    ));
}

#[tokio::test]
async fn txn_staging_merges_own_writes_and_reads_at_read_ts() {
    let shared = setup().await;
    exec(&shared, "INSERT INTO t (id, v) VALUES (1, 'one')").await;

    let mut sess = SqlSession {
        txn: Some(crate::sql::tx::begin(&shared.sql_ts)),
        ..Default::default()
    };
    let stmt = |sql: &str| parse_statement(sql).unwrap();
    write_in(
        &shared,
        &mut sess,
        stmt("INSERT INTO t (id, v) VALUES (2, 'two')"),
    )
    .await
    .unwrap();
    write_in(
        &shared,
        &mut sess,
        stmt("UPDATE t SET v = 'ONE' WHERE id = 1"),
    )
    .await
    .unwrap();
    write_in(&shared, &mut sess, stmt("DELETE FROM t WHERE id = 2"))
        .await
        .unwrap();

    // nothing reached the store yet: another session sees the seed only
    let (_, other) = rows(&shared, "SELECT id, v FROM t ORDER BY id").await;
    assert_eq!(other, vec![vec![Value::Int(1), Value::Str("one".into())]]);

    // the staging session sees own writes merged over the snapshot
    let Statement::Select(q) = stmt("SELECT id, v FROM t ORDER BY id") else {
        panic!("select");
    };
    let (_, mine) = select::run(&shared, &sess, q).await.unwrap();
    assert_eq!(mine, vec![vec![Value::Int(1), Value::Str("ONE".into())]]);
}

#[tokio::test]
async fn txn_commit_flushes_and_rollback_discards() {
    let shared = setup().await;
    exec(&shared, "INSERT INTO t (id, v) VALUES (1, 'one')").await;

    // commit path: staged writes become one committed batch
    let mut sess = SqlSession {
        txn: Some(crate::sql::tx::begin(&shared.sql_ts)),
        ..Default::default()
    };
    write_in(
        &shared,
        &mut sess,
        parse_statement("INSERT INTO t (id, v) VALUES (2, 'two')").unwrap(),
    )
    .await
    .unwrap();
    crate::sql::tx::commit(&shared, sess.txn.take().unwrap())
        .await
        .unwrap();
    let (_, got) = rows(&shared, "SELECT id FROM t ORDER BY id").await;
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);

    // rollback path: staged writes vanish
    let mut sess = SqlSession {
        txn: Some(crate::sql::tx::begin(&shared.sql_ts)),
        ..Default::default()
    };
    write_in(
        &shared,
        &mut sess,
        parse_statement("DELETE FROM t").unwrap(),
    )
    .await
    .unwrap();
    crate::sql::tx::rollback(&shared.sql_ts, sess.txn.take().unwrap());
    let (_, after) = rows(&shared, "SELECT id FROM t ORDER BY id").await;
    assert_eq!(after.len(), 2, "rollback discarded the staged delete");
}
