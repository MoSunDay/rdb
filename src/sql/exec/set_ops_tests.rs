//! Set-operation unit tests: arity/type widening, UNION dedup,
//! INTERSECT / EXCEPT DISTINCT + ALL multiset arithmetic, and the
//! shared column-merge rules (see `set_ops.rs`).

use super::*;
use crate::sql::exec::ColMeta;
use crate::sql::storage::schema::{SqlType, Value};

fn col(name: &str, ty: SqlType) -> ColMeta {
    ColMeta::computed("", name, ty)
}

fn rel(cols: Vec<ColMeta>, rows: Vec<Vec<Value>>) -> Relation {
    Relation::new(cols, rows)
}

#[test]
fn union_all_concatenates_and_keeps_duplicates() {
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![vec![Value::Int(1)], vec![Value::Int(1)]],
    );
    let r = rel(vec![col("b", SqlType::Int)], vec![vec![Value::Int(1)]]);
    let out = merge_relations(SetOp::Union, l, r, true).unwrap();
    assert_eq!(out.rows.len(), 3);
    // left operand names the output column.
    assert_eq!(out.columns[0].name, "a");
}

#[test]
fn union_distinct_dedups_rows_and_nulls() {
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![vec![Value::Null], vec![Value::Int(1)], vec![Value::Null]],
    );
    let r = rel(
        vec![col("b", SqlType::Int)],
        vec![vec![Value::Int(1)], vec![Value::Int(2)]],
    );
    let out = merge_relations(SetOp::Union, l, r, false).unwrap();
    let vals: Vec<&Value> = out.rows.iter().map(|r| &r[0]).collect();
    assert_eq!(vals, vec![&Value::Null, &Value::Int(1), &Value::Int(2)]);
}

#[test]
fn union_arity_mismatch_and_type_widen() {
    let l1 = rel(vec![col("a", SqlType::Int)], vec![vec![Value::Int(1)]]);
    let r2 = rel(
        vec![col("x", SqlType::Int), col("y", SqlType::Int)],
        vec![vec![Value::Int(1), Value::Int(2)]],
    );
    assert!(merge_relations(SetOp::Union, l1.clone(), r2, true).is_err());

    let rd = rel(
        vec![col("d", SqlType::Double)],
        vec![vec![Value::Double(1.5)]],
    );
    let out = merge_relations(SetOp::Union, l1, rd, true).unwrap();
    assert_eq!(out.columns[0].sql_type, SqlType::Double);

    let rs = rel(
        vec![col("s", SqlType::VarChar)],
        vec![vec![Value::Str("x".into())]],
    );
    let l1 = rel(vec![col("a", SqlType::Int)], vec![vec![Value::Int(1)]]);
    assert!(merge_relations(SetOp::Union, l1, rs, true).is_err());
}

#[test]
fn union_widens_date_to_datetime_and_lifts_cells() {
    use crate::sql::temporal::MICROS_PER_DAY;
    let day = rel(
        vec![col("d", SqlType::Date)],
        vec![vec![Value::Date(19_782)]],
    );
    let stamp = rel(
        vec![col("t", SqlType::DateTime)],
        vec![vec![Value::DateTime(19_783 * MICROS_PER_DAY)]],
    );
    // date | datetime -> datetime, and the Date cell lifts to
    // midnight microseconds so rendering keeps full precision.
    let out = merge_relations(SetOp::Union, day.clone(), stamp.clone(), true).unwrap();
    assert_eq!(out.columns[0].sql_type, SqlType::DateTime);
    assert_eq!(
        out.rows.as_slice(),
        &[
            vec![Value::DateTime(19_782 * MICROS_PER_DAY)],
            vec![Value::DateTime(19_783 * MICROS_PER_DAY)]
        ]
    );
    // operand order does not matter
    let out = merge_relations(SetOp::Union, stamp, day, true).unwrap();
    assert_eq!(out.columns[0].sql_type, SqlType::DateTime);
    // plain UNION dedups the widened midnight pair
    let l = rel(
        vec![col("d", SqlType::Date)],
        vec![vec![Value::Date(19_782)]],
    );
    let r = rel(
        vec![col("t", SqlType::DateTime)],
        vec![vec![Value::DateTime(19_782 * MICROS_PER_DAY)]],
    );
    let out = merge_relations(SetOp::Union, l, r, false).unwrap();
    assert_eq!(
        out.rows.as_slice(),
        &[vec![Value::DateTime(19_782 * MICROS_PER_DAY)]]
    );
    // temporal never mixes with numerics or text
    let nums = rel(vec![col("n", SqlType::Int)], vec![vec![Value::Int(1)]]);
    let l = rel(vec![col("d", SqlType::Date)], vec![vec![Value::Date(0)]]);
    assert!(merge_relations(SetOp::Union, l, nums, true).is_err());
}

#[test]
fn intersect_distinct_dedups_and_keeps_common_rows() {
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(2)],
        ],
    );
    let r = rel(
        vec![col("b", SqlType::Int)],
        vec![vec![Value::Int(1)], vec![Value::Int(3)]],
    );
    let out = merge_relations(SetOp::Intersect, l, r, false).unwrap();
    assert_eq!(out.rows.as_slice(), &[vec![Value::Int(1)]]);
    // NULLs equal: a NULL on both sides survives INTERSECT.
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![vec![Value::Null], vec![Value::Int(5)]],
    );
    let r = rel(vec![col("b", SqlType::Int)], vec![vec![Value::Null]]);
    let out = merge_relations(SetOp::Intersect, l, r, false).unwrap();
    assert_eq!(out.rows.as_slice(), &[vec![Value::Null]]);
}

#[test]
fn intersect_all_keeps_min_multiplicities_in_left_order() {
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(2)],
        ],
    );
    let r = rel(
        vec![col("b", SqlType::Int)],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(3)],
        ],
    );
    // min(3, 2) = 2 copies of 1; 2 and 3 drop.
    let out = merge_relations(SetOp::Intersect, l, r, true).unwrap();
    assert_eq!(
        out.rows.as_slice(),
        &[vec![Value::Int(1)], vec![Value::Int(1)]]
    );
}

#[test]
fn except_distinct_drops_right_rows() {
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(2)],
        ],
    );
    let r = rel(vec![col("b", SqlType::Int)], vec![vec![Value::Int(1)]]);
    let out = merge_relations(SetOp::Except, l, r, false).unwrap();
    assert_eq!(out.rows.as_slice(), &[vec![Value::Int(2)]]);
    // NULL on the right kills a NULL on the left (NULLs equal here).
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![vec![Value::Null], vec![Value::Int(7)]],
    );
    let r = rel(vec![col("b", SqlType::Int)], vec![vec![Value::Null]]);
    let out = merge_relations(SetOp::Except, l, r, false).unwrap();
    assert_eq!(out.rows.as_slice(), &[vec![Value::Int(7)]]);
}

#[test]
fn except_all_subtracts_multiplicities() {
    let l = rel(
        vec![col("a", SqlType::Int)],
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(2)],
        ],
    );
    let r = rel(vec![col("b", SqlType::Int)], vec![vec![Value::Int(1)]]);
    // 3 - 1 = 2 copies of 1; 2 untouched (0 on the right).
    let out = merge_relations(SetOp::Except, l, r, true).unwrap();
    assert_eq!(
        out.rows.as_slice(),
        &[
            vec![Value::Int(1)],
            vec![Value::Int(1)],
            vec![Value::Int(2)]
        ]
    );
}

#[test]
fn setop_arity_and_type_errors_name_the_operator() {
    let l1 = rel(vec![col("a", SqlType::Int)], vec![vec![Value::Int(1)]]);
    let r2 = rel(
        vec![col("x", SqlType::Int), col("y", SqlType::Int)],
        vec![vec![Value::Int(1), Value::Int(2)]],
    );
    let e = merge_relations(SetOp::Intersect, l1.clone(), r2, false).unwrap_err();
    assert!(e.msg.contains("INTERSECT operands"), "{e}");
    let r2 = rel(
        vec![col("x", SqlType::Int), col("y", SqlType::Int)],
        vec![vec![Value::Int(1), Value::Int(2)]],
    );
    let e = merge_relations(SetOp::Except, l1, r2, true).unwrap_err();
    assert!(e.msg.contains("EXCEPT operands"), "{e}");
}

#[test]
fn setops_widen_before_row_equality() {
    // Int 1 vs Double 1.0: the Int cell lifts to Double, so the rows
    // compare equal for INTERSECT and vanish from EXCEPT.
    let l = rel(vec![col("a", SqlType::Int)], vec![vec![Value::Int(1)]]);
    let r = rel(
        vec![col("d", SqlType::Double)],
        vec![vec![Value::Double(1.0)]],
    );
    let out = merge_relations(SetOp::Intersect, l.clone(), r.clone(), false).unwrap();
    assert_eq!(out.columns[0].sql_type, SqlType::Double);
    // the surviving cell keeps the left operand's Int (the merged
    // column type drives rendering, same as UNION ALL)
    assert_eq!(out.rows.as_slice(), &[vec![Value::Int(1)]]);
    let out = merge_relations(SetOp::Except, l, r, false).unwrap();
    assert!(out.rows.is_empty());
}

// ---- full-SQL set-operation tests (through the compound pipeline) ----

use crate::sql::exec::select;
use crate::sql::exec::{ddl, write, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::parse_statement;
use crate::state::testutil;

/// n(v): 1, 1, 2, 3 (duplicates on the left); m(v): 1, 3, 3, 5.
async fn setup_sql() -> crate::state::Shared {
    let shared = testutil::shared_with(testutil::test_config());
    for sql in [
        "CREATE TABLE n (v BIGINT PRIMARY KEY, w BIGINT)",
        "CREATE TABLE m (v BIGINT PRIMARY KEY, w BIGINT)",
    ] {
        ddl::run(&shared, parse_statement(sql).unwrap())
            .await
            .unwrap();
    }
    for sql in [
        "INSERT INTO n (v, w) VALUES (1, 0), (2, 0), (3, 0), (4, 0)",
        "INSERT INTO m (v, w) VALUES (1, 0), (3, 0), (4, 0), (5, 0)",
    ] {
        write::insert(
            &shared,
            &mut SqlSession::default(),
            parse_statement(sql).unwrap(),
        )
        .await
        .unwrap();
    }
    shared
}

async fn sql_col(shared: &crate::state::Shared, sql: &str) -> Vec<Value> {
    let Statement::SelectCompound(cq) = parse_statement(sql).unwrap() else {
        panic!("compound: {sql}");
    };
    super::run_statement(shared, &SqlSession::default(), &cq)
        .await
        .unwrap()
        .1
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

#[tokio::test]
async fn sql_intersect_distinct_and_all() {
    let shared = setup_sql().await;
    // DISTINCT: common unique values, sorted output
    let got = sql_col(&shared, "SELECT v FROM n INTERSECT SELECT v FROM m").await;
    assert_eq!(got, vec![Value::Int(1), Value::Int(3), Value::Int(4)]);
    // ALL: min multiplicities (1x1, 1x3, 1x4), left operand order
    let got = sql_col(&shared, "SELECT v FROM n INTERSECT ALL SELECT v FROM m").await;
    assert_eq!(got, vec![Value::Int(1), Value::Int(3), Value::Int(4)]);
}

#[tokio::test]
async fn sql_except_distinct_and_all() {
    let shared = setup_sql().await;
    let got = sql_col(&shared, "SELECT v FROM n EXCEPT SELECT v FROM m").await;
    assert_eq!(got, vec![Value::Int(2)]);
    let got = sql_col(&shared, "SELECT v FROM n EXCEPT ALL SELECT v FROM m").await;
    assert_eq!(got, vec![Value::Int(2)]);
}

#[tokio::test]
async fn sql_mixed_chain_and_tail() {
    let shared = setup_sql().await;
    // left-assoc: ((n UNION ALL m) EXCEPT m) -> concat then remove
    let got = sql_col(
        &shared,
        "SELECT v FROM n UNION ALL SELECT v FROM m EXCEPT SELECT v FROM m",
    )
    .await;
    assert_eq!(got, vec![Value::Int(2)], "left-fold: (n ∪ m) − m");
    // tail ORDER BY / LIMIT applies to the whole compound
    let got = sql_col(
        &shared,
        "SELECT v FROM n INTERSECT SELECT v FROM m ORDER BY 1 DESC LIMIT 2",
    )
    .await;
    assert_eq!(got, vec![Value::Int(4), Value::Int(3)]);
    // column count mismatch names the operator
    let Statement::SelectCompound(cq) =
        parse_statement("SELECT v, w FROM n EXCEPT SELECT v FROM m").unwrap()
    else {
        panic!("compound");
    };
    let e = super::run_statement(&shared, &SqlSession::default(), &cq)
        .await
        .unwrap_err();
    assert!(e.msg.contains("EXCEPT operands"), "{e}");
    // EXPLAIN renders the new operators like UNION always has
    let stmt = parse_statement("SELECT v FROM n INTERSECT ALL SELECT v FROM m").unwrap();
    let crate::sql::exec::ExecOutcome::Rows { rows, .. } = select::explain(&shared, &stmt).unwrap()
    else {
        panic!("explain rows");
    };
    let rendered = format!("{rows:?}");
    assert!(rendered.contains("Intersect-All"), "{rendered}");
}
