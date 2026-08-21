use super::*;
use crate::sql::exec::{ddl, write, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::parse_statement;
use crate::state::testutil;

/// Engine with `t(id BIGINT PK, v VARCHAR NULL)` holding
/// (1,'b'), (2,NULL), (3,'a'), (4,NULL).
async fn setup() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)").unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement("INSERT INTO t (id, v) VALUES (1, 'b'), (2, NULL), (3, 'a'), (4, NULL)")
            .unwrap(),
    )
    .await
    .unwrap();
    shared
}

async fn select_all(shared: &crate::state::Shared, sql: &str) -> (Vec<ColMeta>, Vec<Vec<Value>>) {
    let Statement::Select(q) = parse_statement(sql).unwrap() else {
        panic!("select");
    };
    run(shared, &SqlSession::default(), q).await.unwrap()
}

async fn col(shared: &crate::state::Shared, sql: &str) -> Vec<Value> {
    select_all(shared, sql)
        .await
        .1
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[tokio::test]
async fn where_drops_null_comparands() {
    // v = 'a' never matches NULL v rows
    let got = col(&setup().await, "SELECT id FROM t WHERE v = 'a'").await;
    assert_eq!(got, vec![Value::Int(3)]);
    // NULL-safe IS NULL keeps them
    let got = col(
        &setup().await,
        "SELECT id FROM t WHERE v IS NULL ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![Value::Int(2), Value::Int(4)]);
}

#[tokio::test]
async fn order_by_treats_null_as_smallest() {
    let asc = col(&setup().await, "SELECT v FROM t ORDER BY v ASC, id ASC").await;
    assert_eq!(
        asc,
        vec![
            Value::Null,
            Value::Null,
            Value::Str("a".into()),
            Value::Str("b".into())
        ]
    );
    // DESC flips both the values and the NULL placement
    let desc = col(&setup().await, "SELECT v FROM t ORDER BY v DESC, id DESC").await;
    assert_eq!(
        desc,
        vec![
            Value::Str("b".into()),
            Value::Str("a".into()),
            Value::Null,
            Value::Null
        ]
    );
}

#[tokio::test]
async fn limit_offset_and_distinct() {
    let got = col(
        &setup().await,
        "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1",
    )
    .await;
    assert_eq!(got, vec![Value::Int(2), Value::Int(3)]);
    // distinct over the projected v: NULL appears once
    let got = col(&setup().await, "SELECT DISTINCT v FROM t ORDER BY v ASC").await;
    assert_eq!(
        got,
        vec![Value::Null, Value::Str("a".into()), Value::Str("b".into())]
    );
}

#[tokio::test]
async fn group_by_aggregates_and_empty_global() {
    let shared = setup().await;
    let (meta, rows) = select_all(
        &shared,
        "SELECT v, COUNT(*) AS n, SUM(id) AS s FROM t GROUP BY v ORDER BY v",
    )
    .await;
    // NULL group first (smallest), SUM skips nothing (ids never NULL)
    assert_eq!(
        rows,
        vec![
            vec![Value::Null, Value::Int(2), Value::Int(6)],
            vec![Value::Str("a".into()), Value::Int(1), Value::Int(3)],
            vec![Value::Str("b".into()), Value::Int(1), Value::Int(1)],
        ]
    );
    assert_eq!(meta[1].name, "n", "alias is the output column name");
    // global aggregate over zero rows: COUNT(*)=0, SUM=NULL
    let (_, rows) = select_all(&shared, "SELECT COUNT(*), SUM(id) FROM t WHERE id > 100").await;
    assert_eq!(rows, vec![vec![Value::Int(0), Value::Null]]);
    // HAVING filters groups
    let got = col(&shared, "SELECT v FROM t GROUP BY v HAVING COUNT(*) > 1").await;
    assert_eq!(got, vec![Value::Null]);
}

#[tokio::test]
async fn join_qualified_columns_and_ambiguity() {
    let shared = setup().await;
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE u (id BIGINT PRIMARY KEY, tag VARCHAR(8) NULL)").unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement("INSERT INTO u (id, tag) VALUES (1, 'x'), (3, 'y')").unwrap(),
    )
    .await
    .unwrap();
    let (_, rows) = select_all(
        &shared,
        "SELECT t.id, u.tag FROM t JOIN u ON t.id = u.id ORDER BY t.id",
    )
    .await;
    assert_eq!(
        rows,
        vec![
            vec![Value::Int(1), Value::Str("x".into())],
            vec![Value::Int(3), Value::Str("y".into())],
        ]
    );
    // bare `id` exists on both sides -> ambiguity is an error
    let Statement::Select(q) = parse_statement("SELECT id FROM t JOIN u ON t.id = u.id").unwrap()
    else {
        panic!("select");
    };
    let read_ts = shared.sql_ts.now();
    let _ = scan::materialize(&shared, &q.from, read_ts, None, None).unwrap();
    let err = run(&shared, &SqlSession::default(), q).await.unwrap_err();
    assert!(err.msg.contains("ambiguous column 'id'"), "{}", err.msg);
    // unknown column
    let Statement::Select(q) = parse_statement("SELECT nope FROM t").unwrap() else {
        panic!("select");
    };
    let err = run(&shared, &SqlSession::default(), q)
        .await
        .expect_err("unknown column must error");
    assert!(err.msg.contains("unknown column"), "{}", err.msg);
}

#[tokio::test]
async fn explain_and_alias_metadata() {
    let shared = setup().await;
    let (meta, _) = select_all(&shared, "SELECT v AS label FROM t LIMIT 1").await;
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].name, "label");
    // wildcard expands to every column, metadata included
    let (meta, rows) = select_all(&shared, "SELECT * FROM t WHERE id = 1").await;
    assert_eq!(meta.len(), 2);
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Str("b".into())]]);
}

/// EXPLAIN headline: single-node topologies keep the plain SeqScan
/// (or planner index) verdict; a ready multi-node cluster plans the
/// same single-table read as scatter-gather, banner on top.
#[test]
fn explain_headline_prefers_gather_in_cluster_mode() {
    let shared = testutil::shared_with(testutil::test_config());
    let Statement::Select(q) = parse_statement("SELECT id FROM t").unwrap() else {
        panic!("select");
    };
    // No cluster: no storage-aware verdict (no catalog entry either).
    assert!(headline_lines(&shared, &q).is_empty());
    *shared.topology.write().unwrap() = crate::topology::refresh("a, b, c");
    assert_eq!(
        headline_lines(&shared, &q),
        vec!["Gather(bands=3)".to_string(), "SeqScan t".to_string()]
    );
}
