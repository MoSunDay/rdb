//! Exact-KNN fast-path integration: the D11 shape end to end --
//! matched runs through the bounded heap, declined shapes (DESC, no
//! LIMIT, GROUP BY) fall back to the generic sort yet stay correct,
//! NULL distances ride first, and EXPLAIN reports the collector node.

use super::*;
use crate::sql::exec::select;
use crate::sql::exec::{ddl, write, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::parse_statement;
use crate::state::testutil;

/// `t(id BIGINT PK, v VECTOR(2) NULL)` with points chosen so cosine,
/// euclidean and dot orderings disagree, plus one NULL row.
async fn setup() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2) NULL)").unwrap(),
    )
    .await
    .unwrap();
    write::insert(
        &shared,
        &mut SqlSession::default(),
        parse_statement(
            "INSERT INTO t (id, v) VALUES (1, '[1,0]'), (2, '[0,1]'), (3, '[1,1]'), \
             (4, '[-1,0]'), (5, '[2,0]'), (6, NULL)",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    shared
}

async fn ids(shared: &crate::state::Shared, sql: &str) -> Vec<Value> {
    let Statement::Select(q) = parse_statement(sql).unwrap() else {
        panic!("select");
    };
    select::run(shared, &SqlSession::default(), q)
        .await
        .unwrap()
        .1
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[tokio::test]
async fn cosine_topk_with_scan_order_ties() {
    let shared = setup().await;
    // distances: id1=0, id5=0 (tie -> scan order), id3=1-1/sqrt(2)
    let got = ids(
        &shared,
        "SELECT id FROM t WHERE v IS NOT NULL          ORDER BY DISTANCE(v, '[1,0]', 'cosine') LIMIT 2",
    )
    .await;
    assert_eq!(got, vec![Value::Int(1), Value::Int(5)]);
}

#[tokio::test]
async fn euclidean_and_dot_pick_different_neighbors() {
    let shared = setup().await;
    let got = ids(
        &shared,
        "SELECT id FROM t WHERE v IS NOT NULL          ORDER BY DISTANCE(v, '[1,0]', 'EUCLIDEAN') LIMIT 1",
    )
    .await;
    assert_eq!(got, vec![Value::Int(1)]);
    // negated dot: id5=-2 closest, then the -1 tie (1 before 3)
    let got = ids(
        &shared,
        "SELECT id FROM t WHERE v IS NOT NULL          ORDER BY DISTANCE(v, '[1,0]', 'dot') LIMIT 2",
    )
    .await;
    assert_eq!(got, vec![Value::Int(5), Value::Int(1)]);
}

#[tokio::test]
async fn swapped_operand_position_matches() {
    let shared = setup().await;
    let got = ids(
        &shared,
        "SELECT id FROM t WHERE v IS NOT NULL          ORDER BY DISTANCE('[1,0]', v, 'euclidean') LIMIT 1",
    )
    .await;
    assert_eq!(got, vec![Value::Int(1)]);
}

#[tokio::test]
async fn null_distance_rows_ride_first() {
    let shared = setup().await;
    // the zero query vector puts every non-NULL at cosine distance 1
    // (tie -> scan order), so the NULL row wins LIMIT 2 alone.
    let got = ids(
        &shared,
        "SELECT id FROM t ORDER BY DISTANCE(v, '[0,0]', 'cosine') LIMIT 2",
    )
    .await;
    assert_eq!(got, vec![Value::Int(6), Value::Int(1)]);
}

#[tokio::test]
async fn filter_and_offset_still_apply() {
    let shared = setup().await;
    let got = ids(
        &shared,
        "SELECT id FROM t WHERE id NOT IN (1, 6) \
         ORDER BY DISTANCE(v, '[1,0]', 'cosine') LIMIT 1",
    )
    .await;
    assert_eq!(got, vec![Value::Int(5)]);
    let got = ids(
        &shared,
        "SELECT id FROM t WHERE v IS NOT NULL \
         ORDER BY DISTANCE(v, '[1,0]', 'cosine') LIMIT 1 OFFSET 1",
    )
    .await;
    assert_eq!(got, vec![Value::Int(5)]);
}

#[tokio::test]
async fn declined_shapes_fall_back_but_stay_correct() {
    let shared = setup().await;
    // DESC is not a KNN: farthest first through the generic sort
    // (NULL last on DESC), so id4 (dot=+1) then id2 (dot=0).
    let got = ids(
        &shared,
        "SELECT id FROM t ORDER BY DISTANCE(v, '[1,0]', 'dot') DESC LIMIT 2",
    )
    .await;
    assert_eq!(got, vec![Value::Int(4), Value::Int(2)]);
    // no LIMIT: generic ASC sort, NULL smallest first
    let got = ids(
        &shared,
        "SELECT id FROM t ORDER BY DISTANCE(v, '[1,0]', 'cosine')",
    )
    .await;
    assert_eq!(
        got,
        vec![
            Value::Int(6),
            Value::Int(1),
            Value::Int(5),
            Value::Int(3),
            Value::Int(2),
            Value::Int(4)
        ]
    );
}

#[tokio::test]
async fn projection_evaluates_for_winners_only() {
    let shared = setup().await;
    let Statement::Select(q) = parse_statement(
        "SELECT id, VECTOR_DIM(v) AS d FROM t WHERE v IS NOT NULL \
         ORDER BY DISTANCE(v, '[1,0]', 'cosine') LIMIT 1",
    )
    .unwrap() else {
        panic!("select");
    };
    let (_, rows) = select::run(&shared, &SqlSession::default(), q)
        .await
        .unwrap();
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(2)]]);
}

#[tokio::test]
async fn dimension_mismatch_is_loud() {
    let shared = setup().await;
    let Statement::Select(q) =
        parse_statement("SELECT id FROM t ORDER BY DISTANCE(v, '[1,0,0]', 'cosine') LIMIT 1")
            .unwrap()
    else {
        panic!("select");
    };
    let err = select::run(&shared, &SqlSession::default(), q)
        .await
        .unwrap_err();
    assert!(err.msg.contains("dimension mismatch"), "{}", err.msg);
}

#[test]
fn matcher_declines_non_knn_shapes() {
    for sql in [
        "SELECT id FROM t ORDER BY DISTANCE(v, '[1,0]', 'cosine') DESC LIMIT 1",
        "SELECT id FROM t ORDER BY DISTANCE(v, '[1,0]', 'cosine')",
        "SELECT id FROM t ORDER BY DISTANCE(v, w, 'cosine') LIMIT 1",
        "SELECT id FROM t ORDER BY id LIMIT 1",
    ] {
        let Statement::Select(q) = parse_statement(sql).unwrap() else {
            panic!("select");
        };
        assert!(match_knn(&q).is_none(), "{sql}");
    }
    // the canonical shape (and the swap) match
    for sql in [
        "SELECT id FROM t ORDER BY DISTANCE(v, '[1,0]', 'cosine') LIMIT 2",
        "SELECT id FROM t ORDER BY DISTANCE('[1,0]', v, 'cosine') LIMIT 2",
    ] {
        let Statement::Select(q) = parse_statement(sql).unwrap() else {
            panic!("select");
        };
        assert!(match_knn(&q).is_some(), "{sql}");
    }
}

#[test]
fn explain_line_names_the_collector() {
    let Statement::Select(q) =
        parse_statement("SELECT id FROM t ORDER BY DISTANCE(v, '[1,0]', 'cosine') LIMIT 2")
            .unwrap()
    else {
        panic!("select");
    };
    assert_eq!(
        explain_line(&q).as_deref(),
        Some("vector_knn_scan t top 2 by cosine")
    );
    let Statement::Select(q) = parse_statement("SELECT id FROM t LIMIT 2").unwrap() else {
        panic!("select");
    };
    assert_eq!(explain_line(&q), None);
}
