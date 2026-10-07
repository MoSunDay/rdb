//! Translate-time semantics of ORDER BY / GROUP BY ordinals, select
//! aliases, LIMIT/OFFSET placeholders, and FROM DUAL.

use super::*;
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::order_limit::{limit_display, limit_u64};
use crate::sql::parse::{bind_placeholders, parse_statement, placeholder_count};
use crate::sql::storage::schema::Value;

fn sel(sql: &str) -> Query {
    let Statement::Select(q) = parse_statement(sql).expect("parse/select") else {
        panic!("shape");
    };
    q
}

#[test]
fn order_by_ordinal_becomes_projection() {
    let q = sel("SELECT a, b + 1 FROM t ORDER BY 2");
    let SelectItem::Expr { expr: second, .. } = &q.items[1] else {
        panic!("item");
    };
    assert_eq!(q.order_by.len(), 1);
    assert!(q.order_by[0].asc);
    assert_eq!(&q.order_by[0].expr, second);
    // DESC survives the substitution
    let q = sel("SELECT a, b FROM t ORDER BY 2 DESC");
    assert!(!q.order_by[0].asc);
}

#[test]
fn composed_numbers_are_not_ordinals() {
    // `1+1`, `-1` and quoted `'1'` stay expressions / constants
    let q = sel("SELECT a FROM t ORDER BY 1 + 1");
    assert!(matches!(&q.order_by[0].expr, Expr::BinaryOp { .. }));
    let q = sel("SELECT a FROM t ORDER BY -1");
    assert!(matches!(&q.order_by[0].expr, Expr::Neg(_)));
    let q = sel("SELECT a FROM t ORDER BY '1'");
    assert_eq!(
        q.order_by[0].expr,
        Expr::Lit(Value::Str("1".into())),
        "a quoted number is a constant, not a position"
    );
}

#[test]
fn order_by_ordinal_out_of_range_is_unknown_column() {
    for (sql, needle) in [
        (
            "SELECT a FROM t ORDER BY 2",
            "Unknown column '2' in 'order clause'",
        ),
        (
            "SELECT a FROM t ORDER BY 0",
            "Unknown column '0' in 'order clause'",
        ),
    ] {
        let e = parse_statement(sql).expect_err("out of range");
        assert_eq!(e.code, ErrorCode::BadField, "{sql}: {e}");
        assert!(e.msg.contains(needle), "{sql}: {e}");
    }
}

#[test]
fn group_by_ordinal_becomes_projection_and_range_checks() {
    let q = sel("SELECT a + 1, b FROM t GROUP BY 1");
    let SelectItem::Expr { expr: first, .. } = &q.items[0] else {
        panic!("item");
    };
    assert_eq!(q.group_by, vec![first.clone()]);
    let e = parse_statement("SELECT a, b FROM t GROUP BY 3").expect_err("out of range");
    assert_eq!(e.code, ErrorCode::BadField);
    assert!(
        e.msg.contains("Unknown column '3' in 'group statement'"),
        "{e}"
    );
}

#[test]
fn order_by_and_having_reference_select_aliases() {
    let q = sel("SELECT a + b AS s FROM t ORDER BY s");
    let SelectItem::Expr { expr, .. } = &q.items[0] else {
        panic!("item");
    };
    assert_eq!(&q.order_by[0].expr, expr);
    // case-insensitive, and usable inside larger expressions
    let q = sel("SELECT a + b AS S FROM t ORDER BY s + 1 DESC");
    assert!(matches!(&q.order_by[0].expr, Expr::BinaryOp { .. }));
    // HAVING substitutes aliases too (aggregates included)
    let q = sel("SELECT COUNT(*) AS cnt FROM t HAVING cnt > 1");
    assert!(matches!(&q.having, Some(Expr::BinaryOp { left, .. })
        if matches!(left.as_ref(), Expr::Agg { .. })));
}

#[test]
fn alias_wins_over_same_named_source_column() {
    // MySQL resolves ORDER BY / HAVING names against the select list
    // first: `b` is the alias of `a` here, not the FROM column b.
    let q = sel("SELECT a AS b, b AS a FROM t ORDER BY b");
    let SelectItem::Expr { expr, .. } = &q.items[0] else {
        panic!("item");
    };
    assert_eq!(&q.order_by[0].expr, expr);
}

#[test]
fn unknown_name_still_fails_at_validation() {
    // no alias match: the identifier passes through untouched and the
    // exec-time unknown-column validation still fires
    let q = sel("SELECT a FROM t ORDER BY nope");
    assert_eq!(
        q.order_by[0].expr,
        Expr::Col {
            table: None,
            name: "nope".into()
        }
    );
}

#[test]
fn ordinal_over_wildcard_is_unsupported() {
    // a `*` width is unknown before execution; reject loudly rather
    // than silently dropping the sort
    let e = parse_statement("SELECT * FROM t ORDER BY 1").expect_err("wildcard");
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
    let e = parse_statement("SELECT * FROM t GROUP BY 2").expect_err("wildcard");
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
}

#[test]
fn ordinal_or_alias_referencing_a_parameter_is_unsupported() {
    // substituting a `?`-bearing projection would duplicate the
    // parameter and desync positional binding counts -- reject
    let e = parse_statement("SELECT ? FROM t ORDER BY 1").expect_err("param projection");
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
    let e = parse_statement("SELECT ? + 1 AS s FROM t ORDER BY s").expect_err("param alias");
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
}

#[test]
fn limit_placeholder_shape_count_and_bind() {
    let mut s = parse_statement("SELECT a FROM t WHERE b = ? LIMIT ? OFFSET ?").unwrap();
    assert_eq!(placeholder_count(&s), 3);
    bind_placeholders(&mut s, &[Value::Int(7), Value::Int(5), Value::Int(2)]).expect("bind");
    let q = match &s {
        Statement::Select(q) => q,
        _ => panic!("shape"),
    };
    // after bind the placeholders ride as literals, coerced at exec
    assert_eq!(
        q.limit,
        Some(LimitValue::Param(Box::new(Expr::Lit(Value::Int(5)))))
    );
    assert_eq!(
        q.offset,
        LimitValue::Param(Box::new(Expr::Lit(Value::Int(2))))
    );
    // numeric literals keep the fast path
    let q = sel("SELECT a FROM t LIMIT 5 OFFSET 2");
    assert_eq!(q.limit, Some(LimitValue::Const(5)));
    assert_eq!(q.offset, LimitValue::Const(2));
}

#[test]
fn limit_comma_form_placeholder_shapes() {
    // MySQL `LIMIT offset, count`: two placeholders would bind
    // limit-then-offset (the `LIMIT ? OFFSET ?` order), silently
    // swapping the user's values -- the shape rejects at parse time.
    let e = parse_statement("SELECT a FROM t LIMIT ?, ?").expect_err("two placeholders");
    assert_eq!(e.code, ErrorCode::Parse, "{e}");
    assert!(
        e.msg.contains("use LIMIT ? OFFSET ?"),
        "error steers to the unambiguous spelling: {e}"
    );

    // `LIMIT 5, ?` == offset 5, placeholder count: the lone `?`
    // binds slot 0 (the Const offset skips its bind slot).
    let mut s = parse_statement("SELECT a FROM t LIMIT 5, ?").unwrap();
    assert_eq!(placeholder_count(&s), 1);
    bind_placeholders(&mut s, &[Value::Int(3)]).expect("bind count");
    let Statement::Select(q) = s else {
        panic!("shape")
    };
    assert_eq!(
        q.limit,
        Some(LimitValue::Param(Box::new(Expr::Lit(Value::Int(3)))))
    );
    assert_eq!(q.offset, LimitValue::Const(5));

    // `LIMIT ?, 5` == placeholder offset, count 5: same single-slot
    // story, the `?` is the offset.
    let mut s = parse_statement("SELECT a FROM t LIMIT ?, 5").unwrap();
    assert_eq!(placeholder_count(&s), 1);
    bind_placeholders(&mut s, &[Value::Int(2)]).expect("bind offset");
    let Statement::Select(q) = s else {
        panic!("shape")
    };
    assert_eq!(q.limit, Some(LimitValue::Const(5)));
    assert_eq!(
        q.offset,
        LimitValue::Param(Box::new(Expr::Lit(Value::Int(2))))
    );

    // the non-comma spelling keeps both placeholders (bound in text
    // order by limit_placeholder_shape_count_and_bind above)
    let q = sel("SELECT a FROM t LIMIT ? OFFSET ?");
    assert!(matches!(q.limit, Some(LimitValue::Param(_))));
    assert!(matches!(q.offset, LimitValue::Param(_)));
}

#[test]
fn limit_u64_coercion_matrix() {
    assert_eq!(limit_u64(&LimitValue::Const(5)), Ok(5));
    assert_eq!(
        limit_u64(&LimitValue::Param(Box::new(Expr::Lit(Value::Int(3))))),
        Ok(3)
    );
    for bad in [
        LimitValue::Param(Box::new(Expr::Lit(Value::Int(-1)))),
        LimitValue::Param(Box::new(Expr::Lit(Value::Double(1.5)))),
        LimitValue::Param(Box::new(Expr::Lit(Value::Str("5".into())))),
        LimitValue::Param(Box::new(Expr::Lit(Value::Null))),
        LimitValue::Param(Box::new(Expr::Placeholder)),
    ] {
        let e = limit_u64(&bad).expect_err("must reject");
        assert_eq!(e.code, ErrorCode::Parse, "{bad:?}: {e}");
        assert!(e.msg.contains("non-negative integer"), "{e}");
    }
    assert_eq!(limit_display(&LimitValue::Const(7)), "7");
    assert_eq!(
        limit_display(&LimitValue::Param(Box::new(Expr::Placeholder))),
        "?"
    );
}

#[test]
fn from_dual_normalizes_to_no_table() {
    for sql in ["SELECT 1 FROM DUAL", "SELECT 2 FROM dual"] {
        let q = sel(sql);
        assert!(matches!(q.from, TableRef::NoTable), "{sql}");
    }
    // WHERE / LIMIT ride along
    let q = sel("SELECT 3 FROM DUAL WHERE 1 = 1 LIMIT 1");
    assert_eq!(q.limit, Some(LimitValue::Const(1)));
    // anything else dual-ish stays an ordinary table name
    let q = sel("SELECT 1 FROM dual2");
    assert!(matches!(q.from, TableRef::Table { name, .. } if name == "dual2"));
    let e = parse_statement("SELECT 1 FROM db.dual").expect_err("qualified dual");
    assert_eq!(e.code, ErrorCode::NotSupported);
}
