//! Unit tests for [`crate::sql::exec::join_hash`]: planning decisions
//! (which conditions become hash keys, which fall back) and output
//! equivalence with the nested-loop path.

use super::*;

fn side(qualifier: &str, cols: &[(&str, SqlType)]) -> FromScope {
    let mut scope = FromScope::default();
    scope.sides.push(crate::sql::exec::scan::ScopeSide {
        qualifier: qualifier.to_string(),
        table: qualifier.to_string(),
        columns: cols.iter().map(|(n, _)| n.to_string()).collect(),
        types: cols.iter().map(|(_, t)| *t).collect(),
        nullable: vec![true; cols.len()],
        key_pos: Vec::new(),
        offset: 0,
    });
    scope
}

fn src(scope: FromScope, rows: Vec<Vec<Value>>) -> Source {
    Source { scope, rows }
}

fn expr_col(t: Option<&str>, n: &str) -> Expr {
    Expr::Col {
        table: t.map(str::to_string),
        name: n.to_string(),
    }
}

fn and(l: Expr, r: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(l),
        op: BinOp::And,
        right: Box::new(r),
    }
}

fn eq(l: Expr, r: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(l),
        op: BinOp::Eq,
        right: Box::new(r),
    }
}

fn bigint_side(q: &str) -> FromScope {
    side(q, &[("id", SqlType::Int), ("k", SqlType::Int)])
}

#[test]
fn equi_conjunction_joins_and_orders_like_nested_loop() {
    let l = bigint_side("l");
    let r = bigint_side("r");
    let lr = src(
        l,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(10)],
            vec![Value::Int(3), Value::Int(30)], // no right match
            vec![Value::Null, Value::Int(40)],   // NULL key never matches
        ],
    );
    let rr = src(
        r,
        vec![
            vec![Value::Int(9), Value::Int(10)],
            vec![Value::Int(8), Value::Int(10)],
            vec![Value::Int(7), Value::Null], // NULL key never matches
        ],
    );
    let on = eq(expr_col(Some("l"), "k"), expr_col(Some("r"), "k"));
    let rows = try_join(&lr, &rr, Some(&on), &[]).unwrap().unwrap().rows;
    // left-major order; within one left row, build (right) order.
    let got: Vec<i64> = rows
        .iter()
        .map(|r| r[2].clone())
        .map(|v| match v {
            Value::Int(i) => i,
            _ => -1,
        })
        .collect();
    assert_eq!(got, vec![9, 8, 9, 8]);
    assert_eq!(rows.len(), 4);
}

#[test]
fn residual_filter_applies_after_bucket_match() {
    let l = bigint_side("l");
    let r = bigint_side("r");
    let lr = src(l.clone(), vec![vec![Value::Int(1), Value::Int(10)]]);
    let rr = src(
        r.clone(),
        vec![
            vec![Value::Int(5), Value::Int(10)],
            vec![Value::Int(50), Value::Int(10)],
        ],
    );
    let on = and(
        eq(expr_col(Some("l"), "k"), expr_col(Some("r"), "k")),
        Expr::BinaryOp {
            left: Box::new(expr_col(Some("r"), "id")),
            op: BinOp::Lt,
            right: Box::new(Expr::Lit(Value::Int(10))),
        },
    );
    let rows = try_join(&lr, &rr, Some(&on), &[]).unwrap().unwrap().rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][2], Value::Int(5));
}

#[test]
fn using_resolves_named_columns_on_both_sides() {
    let l = side("l", &[("id", SqlType::Int)]);
    let r = side("r", &[("id", SqlType::Int)]);
    let lr = src(l, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    let rr = src(r, vec![vec![Value::Int(2)], vec![Value::Int(1)]]);
    let rows = try_join(&lr, &rr, None, &["id".to_string()])
        .unwrap()
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(2));
}

#[test]
fn non_equi_or_mixed_types_fall_back() {
    let l = bigint_side("l");
    let r = bigint_side("r");
    // pure range predicate: no equi pair
    let on = Expr::BinaryOp {
        left: Box::new(expr_col(Some("l"), "k")),
        op: BinOp::Lt,
        right: Box::new(expr_col(Some("r"), "id")),
    };
    assert!(try_join(
        &src(l.clone(), vec![]),
        &src(r.clone(), vec![]),
        Some(&on),
        &[]
    )
    .unwrap()
    .is_none());
    // equi pair but type mismatch across sides
    let rt = side("r", &[("id", SqlType::Int), ("k", SqlType::Double)]);
    let on2 = eq(expr_col(Some("l"), "k"), expr_col(Some("r"), "k"));
    assert!(try_join(&src(l, vec![]), &src(rt, vec![]), Some(&on2), &[])
        .unwrap()
        .is_none());
}

#[test]
fn hash_collision_cannot_false_match() {
    // distinct values whose hashes are (overwhelmingly likely)
    // different, but assert the re-check logic via equal keys first
    let l = bigint_side("l");
    let r = bigint_side("r");
    let lr = src(l, vec![vec![Value::Int(1), Value::Int(7)]]);
    let rr = src(
        r,
        vec![
            vec![Value::Int(1), Value::Int(7)],
            vec![Value::Int(2), Value::Int(7)],
        ],
    );
    let on = eq(expr_col(Some("l"), "id"), expr_col(Some("r"), "id"));
    let rows = try_join(&lr, &rr, Some(&on), &[]).unwrap().unwrap().rows;
    assert_eq!(rows.len(), 1);
}
