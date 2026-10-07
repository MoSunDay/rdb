//! Subquery behavior through the full SELECT pipeline: uncorrelated
//! pre-materialization (scalar / IN / EXISTS), SQL three-valued NOT IN
//! traps, and correlated binding per outer row (scalar, IN, EXISTS,
//! nesting, aggregates, and the loud-reject shapes).

use super::*;
use crate::sql::exec::set_ops;
use crate::sql::exec::{ddl, write};
use crate::sql::exec::{select, ColMeta, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::parse::parse_statement;
use crate::state::testutil;

fn i(n: i64) -> Value {
    Value::Int(n)
}

/// t(id PK, v): (1,'b'), (2,NULL), (3,'a'), (4,NULL)
/// s(k PK, x): (1,1), (2,NULL), (3,33)
/// d(k, x): duplicate k's for multiset / multi-row cases.
async fn setup() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    for ddl_sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
        "CREATE TABLE s (k BIGINT PRIMARY KEY, x BIGINT NULL)",
        "CREATE TABLE d (id BIGINT PRIMARY KEY, k BIGINT, x BIGINT NULL)",
    ] {
        ddl::run(&shared, parse_statement(ddl_sql).unwrap())
            .await
            .unwrap();
    }
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement("INSERT INTO t (id, v) VALUES (1, 'b'), (2, NULL), (3, 'a'), (4, NULL)")
            .unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement("INSERT INTO s (k, x) VALUES (1, 1), (2, NULL), (3, 33)").unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement(
            "INSERT INTO d (id, k, x) VALUES (1, 1, 10), (2, 2, 11), (3, 2, NULL), (4, 3, 33)",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    shared
}

async fn run_sql(
    shared: &crate::state::Shared,
    sql: &str,
) -> SqlResult<(Vec<ColMeta>, Vec<Vec<Value>>)> {
    // both statement shapes execute through the same pipeline
    match parse_statement(sql).unwrap() {
        Statement::Select(q) => select::run(shared, &SqlSession::default(), q).await,
        Statement::SelectCompound(cq) => {
            set_ops::run_statement(shared, &SqlSession::default(), &cq).await
        }
        _ => panic!("select: {sql}"),
    }
}

async fn col(shared: &crate::state::Shared, sql: &str) -> Vec<Value> {
    run_sql(shared, sql)
        .await
        .unwrap()
        .1
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

async fn col_err(shared: &crate::state::Shared, sql: &str) -> SqlError {
    run_sql(shared, sql).await.unwrap_err()
}

#[tokio::test]
async fn uncorrelated_exists_folds_to_boolean() {
    let shared = setup().await;
    // non-empty inner -> every row passes; empty inner -> none
    let got = col(&shared, "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s)").await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(4)]);
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE k = 99)",
    )
    .await;
    assert!(got.is_empty());
    // NOT EXISTS mirrors both, and EXISTS ignores column count
    let got = col(
        &shared,
        "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM s WHERE k = 99)",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(4)]);
    let got = col(
        &shared,
        "SELECT id FROM t WHERE NOT EXISTS (SELECT k, x FROM s WHERE k = 99)",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(4)]);
    // EXPLAIN renders the EXISTS predicate like any other expression
    let stmt = parse_statement("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE s.k = t.id)")
        .unwrap();
    let crate::sql::exec::ExecOutcome::Rows { rows, .. } = select::explain(&shared, &stmt).unwrap()
    else {
        panic!("explain rows");
    };
    let text = format!("{rows:?}");
    assert!(text.contains("EXISTS (SELECT ...)"), "{text}");
}

#[tokio::test]
async fn correlated_exists_is_a_semijoin() {
    let shared = setup().await;
    // s.k covers t ids 1..3; t.id 4 has no match
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE s.k = t.id)",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3)]);
    let got = col(
        &shared,
        "SELECT id FROM t WHERE NOT EXISTS (SELECT 1 FROM s WHERE s.k = t.id)",
    )
    .await;
    assert_eq!(got, vec![i(4)]);
    // the bound predicate also drives the locking-read matcher
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE s.k = t.id) FOR UPDATE",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3)]);
    // an aggregate over the correlated scope: COUNT(*) without GROUP
    // BY always yields exactly one row -> EXISTS is true everywhere
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT COUNT(*) FROM s WHERE s.k = t.id)",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(4)]);
}

#[tokio::test]
async fn correlated_scalar_in_projection_and_filter() {
    let shared = setup().await;
    // empty -> NULL (t.id 4 has no s row; t.id 2's row has x NULL)
    let got = col(
        &shared,
        "SELECT (SELECT MAX(x) FROM s WHERE s.k = t.id) FROM t ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(1), Value::Null, i(33), Value::Null]);
    // NULL propagates through comparisons in the outer WHERE
    let got = col(
        &shared,
        "SELECT id FROM t WHERE (SELECT MAX(x) FROM s WHERE s.k = t.id) > 5 ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(3)]);
    // SUM over the correlated scope binds per outer row too
    let got = col(
        &shared,
        "SELECT (SELECT SUM(x) FROM d WHERE d.k = t.id) FROM t ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(10), i(11), i(33), Value::Null]);
}

#[tokio::test]
async fn correlated_scalar_multi_row_is_error_1242_style() {
    let shared = setup().await;
    // d has two rows for k = 2: binding t.id = 2 explodes loudly
    let e = col_err(&shared, "SELECT (SELECT x FROM d WHERE d.k = t.id) FROM t").await;
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("more than 1 row"), "{e}");
}

#[tokio::test]
async fn correlated_in_and_not_in() {
    let shared = setup().await;
    // per-binding member sets: id 1 -> {1} (hit), 2 -> {NULL}
    // (unknown), 3 -> {33} (miss), 4 -> {} (miss)
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id IN (SELECT x FROM s WHERE s.k = t.id) ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(1)]);
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id NOT IN (SELECT x FROM s WHERE s.k = t.id) ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(3), i(4)]);
}

#[tokio::test]
async fn not_in_empty_set_and_null_traps() {
    let shared = setup().await;
    // empty member set: NOT IN is TRUE for every row
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id NOT IN (SELECT x FROM s WHERE k = 99) ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(4)]);
    // a NULL member makes NOT IN never TRUE (unknown)
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id NOT IN (SELECT x FROM s)",
    )
    .await;
    assert!(got.is_empty());
    let got = col(&shared, "SELECT id FROM t WHERE id NOT IN (10, NULL)").await;
    assert!(got.is_empty());
    // literal lists: no NULL -> plain negation
    let got = col(&shared, "SELECT id FROM t WHERE id NOT IN (10) ORDER BY id").await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(4)]);
    // and IN with a NULL member still matches its equals
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id IN (1, NULL) ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(1)]);
}

#[tokio::test]
async fn correlated_nests_two_levels() {
    let shared = setup().await;
    // inner-most correlates to the MIDDLE scope (s), the middle to t:
    // each level binds against its own outer rows
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (\
         SELECT 1 FROM s WHERE s.k = t.id AND x > (SELECT AVG(x) FROM d WHERE d.k = s.k)\
         ) ORDER BY id",
    )
    .await;
    // s(1, x=1): d.k=1 avg 10 -> 1 > 10 false; s(3, x=33): d.k=3 avg
    // 33 -> false (equal); s(2, x=NULL) unknown -> only t.id 2 not...
    // every binding false/unknown -> empty
    assert!(got.is_empty());
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (\
         SELECT 1 FROM s WHERE s.k = t.id AND x >= (SELECT MIN(x) FROM d WHERE d.k = s.k)\
         ) ORDER BY id",
    )
    .await;
    // s(1): 1 >= 10 false; s(2): x NULL unknown; s(3): 33 >= 33 true
    assert_eq!(got, vec![i(3)]);
}

#[tokio::test]
async fn correlated_inside_setop_arm_binds_per_arm() {
    let shared = setup().await;
    let got = col(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE s.k = t.id) UNION ALL SELECT 99",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3), i(99)]);
}

#[tokio::test]
async fn correlated_join_condition_rejects_loudly() {
    let shared = setup().await;
    // uncorrelated subqueries in JOIN conditions now rewrite (bonus)
    let got = col(
        &shared,
        "SELECT t.id FROM t JOIN s ON t.id = s.k AND s.k = (SELECT MAX(k) FROM s)",
    )
    .await;
    assert_eq!(got, vec![i(3)]);
    // correlated ones cannot bind (no outer rows during FROM
    // materialization) -> explicit unsupported error
    let e = col_err(
        &shared,
        "SELECT t.id FROM t JOIN s ON s.k = (SELECT MAX(x) FROM d WHERE d.k = t.id)",
    )
    .await;
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("JOIN conditions"), "{e}");
}

#[tokio::test]
async fn unresolvable_names_stay_honest_errors() {
    let shared = setup().await;
    // typo in the inner scope: not an outer reference anywhere
    let e = col_err(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE s.zz = 1)",
    )
    .await;
    assert!(e.msg.contains("unknown column 's.zz'"), "{e}");
    // typo in the outer direction (nothing binds t.nope): deferred,
    // no outer reference resolves, the re-run reports it verbatim
    let e = col_err(
        &shared,
        "SELECT id FROM t WHERE EXISTS (SELECT 1 FROM s WHERE s.k = t.nope)",
    )
    .await;
    assert!(e.msg.contains("unknown column 't.nope'"), "{e}");
}

#[tokio::test]
async fn uncorrelated_scalar_and_in_still_fold() {
    let shared = setup().await;
    // regression: the pre-materialization path (M2) is unchanged
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id = (SELECT MAX(k) FROM s)",
    )
    .await;
    assert_eq!(got, vec![i(3)]);
    let got = col(
        &shared,
        "SELECT id FROM t WHERE id IN (SELECT k FROM s) ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(3)]);
}

/// Second output column of each row (row order pinned by ORDER BY on
/// the projected key): the correlated-count cases pair the outer key
/// with its per-row subquery answer.
async fn col1(shared: &crate::state::Shared, sql: &str) -> Vec<Value> {
    run_sql(shared, sql)
        .await
        .unwrap()
        .1
        .into_iter()
        .map(|mut r| r.remove(1))
        .collect()
}

#[tokio::test]
async fn inner_unqualified_name_binds_inner_table_not_outer() {
    let shared = setup().await;
    // The inner FROM (d) carries its own k -- duplicate values 1,2,2,3
    // -- and the outer s has a k at the same outer index. The inner
    // unqualified k must read the INNER column, so the counts vary per
    // outer row. The bug substituted BOTH sides of `k = s.k` with the
    // same outer literal, folding the predicate to lit = lit and
    // counting all of d for every row (4,4,4).
    let got = col1(
        &shared,
        "SELECT s.k, (SELECT COUNT(*) FROM d WHERE k = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
    // Same shape with the inner table aliased: the alias qualifier
    // shadows the name just the same.
    let got = col1(
        &shared,
        "SELECT s.k, (SELECT COUNT(*) FROM d AS dd WHERE k = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
}

#[tokio::test]
async fn derived_table_columns_shadow_outer_names() {
    let shared = setup().await;
    // The derived side exposes x (= d.k values 1,2,2,3); the outer s
    // ALSO has an x (1, NULL, 33). The unqualified x inside the
    // subquery must read the DERIVED column (1,2,1); the old shadow
    // set saw no side at all for a derived table, bound x to the
    // outer literal and folded the predicate to lit = lit (4,0,0).
    let got = col1(
        &shared,
        "SELECT s.k, (SELECT COUNT(*) FROM (SELECT k AS x FROM d) p WHERE x = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
    // A wildcard derives the inner FROM's side columns the same way
    // (superset: the derived relation exposes exactly those).
    let got = col1(
        &shared,
        "SELECT s.k, (SELECT COUNT(*) FROM (SELECT * FROM d) p WHERE k = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
    // An unaliased expression is not derivable: the side degrades to
    // qualifier-only. The derived relation exposes only the rendered
    // `k + 0`, so x below is a GENUINE outer reference (s.x) and p.k
    // fails the inner run loudly instead of leaking outer data.
    let got = col1(
        &shared,
        "SELECT s.k, (SELECT COUNT(*) FROM (SELECT k + 0 FROM d) p WHERE x = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(4), i(0), i(0)]);
    let e = col_err(
        &shared,
        "SELECT s.k, (SELECT COUNT(*) FROM (SELECT k + 0 FROM d) p WHERE p.k = s.k) FROM s",
    )
    .await;
    assert!(e.msg.contains("unknown column 'p.k'"), "{e}");
}

#[tokio::test]
async fn subquery_cte_output_shadows_outer_names() {
    let shared = setup().await;
    // The subquery's own WITH: c exposes x (= d.k values 1,2,2,3),
    // the outer s also has an x. The unqualified x must read c's
    // output column (1,2,1), not the outer literal (4,0,0).
    let got = col1(
        &shared,
        "SELECT s.k, (WITH c AS (SELECT k AS x FROM d) SELECT COUNT(*) FROM c WHERE x = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
    // A positional CTE alias list names the output columns too.
    let got = col1(
        &shared,
        "SELECT s.k, (WITH c (x) AS (SELECT k FROM d) SELECT COUNT(*) FROM c WHERE x = s.k) FROM s ORDER BY s.k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
    // An OUTER CTE referenced inside the subquery's FROM: the
    // materialized relation's exact output names shadow.
    let got = col1(
        &shared,
        "WITH c AS (SELECT k AS x FROM d) SELECT s.k, (SELECT COUNT(*) FROM c WHERE x = s.k) FROM s ORDER BY k",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1)]);
}

#[tokio::test]
async fn genuinely_outer_refs_still_bind_and_memoize() {
    let shared = setup().await;
    // Regression: the shadow widening must not swallow real outer
    // references -- qualified d.k binds per outer row (memoized per
    // distinct binding), and an unqualified name the inner scope
    // lacks (v against inner s) still binds the outer t.
    let got = col1(
        &shared,
        "SELECT t.id, (SELECT COUNT(*) FROM d WHERE d.k = t.id) FROM t ORDER BY t.id",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2), i(1), i(0)]);
    let got = col1(
        &shared,
        "SELECT t.id, (SELECT COUNT(*) FROM s WHERE v = 'b') FROM t ORDER BY t.id",
    )
    .await;
    assert_eq!(got, vec![i(3), i(0), i(0), i(0)]);
}
