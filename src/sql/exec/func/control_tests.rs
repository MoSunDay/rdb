//! Control-family tests: VERSION, and the CASE/CAST semantics routed
//! through here (CASE lazily, CAST over already-evaluated values).

use super::*;
use crate::sql::exec::expr::eval as eval_expr;
use crate::sql::exec::expr::ColumnScope;
use crate::sql::parse::ast::{BinOp, CastSpec, Expr};
use crate::sql::parse::error::SqlResult;

struct Scope;
impl ColumnScope for Scope {
    fn resolve(&self, _t: Option<&str>, n: &str) -> Option<usize> {
        match n {
            "a" => Some(0),
            _ => None,
        }
    }
}

fn eval_str(e: &Expr) -> SqlResult<Value> {
    eval_expr(e, &Scope, &[Value::Int(1)])
}

#[test]
fn version_is_the_crate_version() {
    assert_eq!(
        eval("version", &[]),
        Some(Ok(Value::Str(env!("CARGO_PKG_VERSION").to_string())))
    );
    assert!(eval("version", &[Value::Null]).unwrap().is_err());
    assert!(eval("if", &[]).is_none(), "part B families");
}

fn case(
    operand: Option<Expr>,
    branches: Vec<(Expr, Expr)>,
    else_expr: Option<Expr>,
) -> SqlResult<Value> {
    eval_str(&Expr::Case {
        operand: operand.map(Box::new),
        branches,
        else_expr: else_expr.map(Box::new),
    })
}

fn lit(v: Value) -> Expr {
    Expr::Lit(v)
}

// Two forms, in-order matching, lazy THEN, and the three-valued rule
// that NULL never matches (neither as operand nor as WHEN value).
#[test]
fn case_searched_and_simple_forms() {
    let branches = vec![
        (lit(Value::Bool(false)), lit(Value::Int(1))),
        (lit(Value::Bool(true)), lit(Value::Int(2))),
    ];
    assert_eq!(
        case(None, branches, Some(lit(Value::Int(3)))),
        Ok(Value::Int(2))
    );
    // No branch true, no ELSE -> NULL.
    assert_eq!(
        case(
            None,
            vec![(lit(Value::Bool(false)), lit(Value::Int(1)))],
            None
        ),
        Ok(Value::Null)
    );
    // WHEN NULL is unknown, not true: the ELSE answers.
    assert_eq!(
        case(
            None,
            vec![(lit(Value::Null), lit(Value::Int(1)))],
            Some(lit(Value::Int(9)))
        ),
        Ok(Value::Int(9))
    );
    // Simple CASE: WHEN NULL never matches a NULL operand.
    assert_eq!(
        case(
            Some(lit(Value::Null)),
            vec![(lit(Value::Null), lit(Value::Int(1)))],
            Some(lit(Value::Int(7)))
        ),
        Ok(Value::Int(7))
    );
    assert_eq!(
        case(
            Some(lit(Value::Int(1))),
            vec![(lit(Value::Int(2)), lit(Value::Int(1)))],
            None
        ),
        Ok(Value::Null)
    );
    assert_eq!(
        case(
            Some(lit(Value::Str("b".into()))),
            vec![(lit(Value::Str("a".into())), lit(Value::Int(1)))],
            None
        ),
        Ok(Value::Null)
    );
}

// The matching THEN is the only result evaluated: an erroring branch
// after the match must never run (lazy short-circuit).
#[test]
fn case_short_circuits_then_branches() {
    let boom = Expr::Func {
        name: "no_such_fn".into(),
        args: vec![],
    };
    let e = Expr::Case {
        operand: None,
        branches: vec![
            (lit(Value::Bool(true)), lit(Value::Int(5))),
            (lit(Value::Bool(true)), boom),
        ],
        else_expr: None,
    };
    assert_eq!(eval_str(&e), Ok(Value::Int(5)));
}

fn cast(v: Value, to: CastSpec) -> SqlResult<Value> {
    eval_str(&Expr::Cast {
        expr: Box::new(lit(v)),
        to,
    })
}

// Signed: Int identity, Str parse (with decimal fallback rounding half
// away from zero), Double/Decimal rounding, temporals compact, NULL.
#[test]
fn cast_signed_matrix() {
    use crate::sql::parse::error::ErrorCode;
    assert_eq!(cast(Value::Null, CastSpec::Signed), Ok(Value::Null));
    assert_eq!(cast(Value::Int(7), CastSpec::Signed), Ok(Value::Int(7)));
    assert_eq!(cast(Value::Bool(true), CastSpec::Signed), Ok(Value::Int(1)));
    assert_eq!(
        cast(Value::Str(" 42 ".into()), CastSpec::Signed),
        Ok(Value::Int(42))
    );
    assert_eq!(
        cast(Value::Str("12.5".into()), CastSpec::Signed),
        Ok(Value::Int(13))
    );
    assert_eq!(
        cast(Value::Str("-12.5".into()), CastSpec::Signed),
        Ok(Value::Int(-13))
    );
    assert_eq!(
        cast(Value::Double(1.5), CastSpec::Signed),
        Ok(Value::Int(2))
    );
    assert_eq!(
        cast(Value::Double(-1.5), CastSpec::Signed),
        Ok(Value::Int(-2))
    );
    assert_eq!(
        cast(Value::Decimal(125, 1), CastSpec::Signed),
        Ok(Value::Int(13))
    );
    // Out of range and junk strings are loud (MySQL 1692/1690 style).
    let over = cast(Value::Str("99999999999999999999".into()), CastSpec::Signed);
    assert_eq!(over.unwrap_err().code, ErrorCode::WrongValue);
    let junk = cast(Value::Str("abc".into()), CastSpec::Signed);
    assert!(junk.unwrap_err().msg.contains("Incorrect integer"));
    // Date exposes its compact form.
    assert_eq!(
        cast(Value::Date(19_782), CastSpec::Signed),
        Ok(Value::Int(20_240_229))
    );
}

// Unsigned: same paths, bounded by [0, i64::MAX] -- the Value domain
// has no u64, so MySQL's wrap-into-huge is a loud error instead.
#[test]
fn cast_unsigned_bounds() {
    assert_eq!(cast(Value::Int(7), CastSpec::Unsigned), Ok(Value::Int(7)));
    let e = cast(Value::Int(-1), CastSpec::Unsigned).unwrap_err();
    assert!(e.msg.contains("UNSIGNED"), "{e}");
    assert_eq!(
        cast(Value::Str("5".into()), CastSpec::Unsigned),
        Ok(Value::Int(5))
    );
}

// CHAR(n): canonical text render then character (not byte) truncate.
#[test]
fn cast_char_truncates_chars() {
    assert_eq!(
        cast(Value::Int(42), CastSpec::Char(None)),
        Ok(Value::Str("42".into()))
    );
    assert_eq!(
        cast(Value::Str("h\u{e9}llo".into()), CastSpec::Char(Some(2))),
        Ok(Value::Str("h\u{e9}".into()))
    );
    assert_eq!(
        cast(Value::Double(1.5), CastSpec::Char(None)),
        Ok(Value::Str("1.5".into()))
    );
    assert_eq!(
        cast(Value::Decimal(-15, 1), CastSpec::Char(None)),
        Ok(Value::Str("-1.5".into()))
    );
    assert_eq!(
        cast(Value::Bool(false), CastSpec::Char(None)),
        Ok(Value::Str("0".into()))
    );
}

// DECIMAL(p,s) rides the exact column-coercion path (fit_column):
// rescale rounds half away from zero, precision bounds reject.
#[test]
fn cast_decimal_reuses_column_coercion() {
    assert_eq!(
        cast(
            Value::Int(999),
            CastSpec::Decimal {
                precision: 5,
                scale: 2
            }
        ),
        Ok(Value::Decimal(99900, 2))
    );
    assert!(cast(
        Value::Decimal(999_995, 3),
        CastSpec::Decimal {
            precision: 5,
            scale: 2
        }
    )
    .unwrap_err()
    .msg
    .contains("Out of range"));
    assert_eq!(
        cast(
            Value::Str("2.675".into()),
            CastSpec::Decimal {
                precision: 18,
                scale: 2
            }
        ),
        Ok(Value::Decimal(268, 2))
    );
}

// CASE over real columns proves the scope-based path (operand reads
// the row), and CAST composes inside a binary operator.
#[test]
fn case_and_cast_compose_with_the_executor() {
    let e = Expr::BinaryOp {
        left: Box::new(Expr::Case {
            operand: Some(Box::new(Expr::Col {
                table: None,
                name: "a".into(),
            })),
            branches: vec![(lit(Value::Int(1)), lit(Value::Int(10)))],
            else_expr: Some(Box::new(lit(Value::Int(20)))),
        }),
        op: BinOp::Add,
        right: Box::new(Expr::Cast {
            expr: Box::new(lit(Value::Str("2".into()))),
            to: CastSpec::Signed,
        }),
    };
    assert_eq!(eval_str(&e), Ok(Value::Int(12)));
}

// ---- part B: the lazy control family (IF/IFNULL/NULLIF/COALESCE) ----

fn call(name: &str, args: Vec<Expr>) -> SqlResult<Value> {
    eval_str(&Expr::Func {
        name: name.to_string(),
        args,
    })
}

// An expression that would ERROR if evaluated (length of an int is a
// loud domain error): proves the untaken branch never runs.
fn poison() -> Expr {
    Expr::Func {
        name: "length".to_string(),
        args: vec![Expr::Lit(Value::Int(5))],
    }
}

#[test]
fn if_is_lazy_and_treats_null_as_false() {
    assert_eq!(
        call(
            "if",
            vec![lit(Value::Bool(true)), lit(Value::Int(1)), poison()]
        ),
        Ok(Value::Int(1))
    );
    assert_eq!(
        call(
            "if",
            vec![lit(Value::Bool(false)), poison(), lit(Value::Int(2))]
        ),
        Ok(Value::Int(2))
    );
    // NULL condition is not TRUE -> the ELSE side.
    assert_eq!(
        call("if", vec![lit(Value::Null), poison(), lit(Value::Int(3))]),
        Ok(Value::Int(3))
    );
    // MySQL numeric-truth: nonzero double is true.
    assert_eq!(
        call(
            "if",
            vec![lit(Value::Double(0.5)), lit(Value::Int(1)), poison()]
        ),
        Ok(Value::Int(1))
    );
    let e = call("if", vec![lit(Value::Bool(true))]).unwrap_err();
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
}

#[test]
fn ifnull_returns_first_non_null_lazily() {
    assert_eq!(
        call(
            "ifnull",
            vec![lit(Value::Null), lit(Value::Str("b".into()))]
        ),
        Ok(Value::Str("b".into()))
    );
    // The first argument wins without touching the second.
    assert_eq!(
        call("ifnull", vec![lit(Value::Int(7)), poison()]),
        Ok(Value::Int(7))
    );
    assert_eq!(
        call("ifnull", vec![lit(Value::Null), lit(Value::Null)]),
        Ok(Value::Null)
    );
}

#[test]
fn nullif_nulls_on_equality() {
    assert_eq!(
        call("nullif", vec![lit(Value::Int(2)), lit(Value::Int(2))]),
        Ok(Value::Null)
    );
    assert_eq!(
        call("nullif", vec![lit(Value::Int(2)), lit(Value::Int(3))]),
        Ok(Value::Int(2))
    );
    // Plain '=' semantics: a NULL on either side answers the first
    // argument (NULL stays NULL; NULL vs 1 does not equal, but a IS
    // NULL so the result is NULL).
    assert_eq!(
        call("nullif", vec![lit(Value::Null), lit(Value::Int(1))]),
        Ok(Value::Null)
    );
    // b is never evaluated when a is NULL.
    assert_eq!(
        call("nullif", vec![lit(Value::Null), poison()]),
        Ok(Value::Null)
    );
    // Strings compare byte-wise.
    assert_eq!(
        call(
            "nullif",
            vec![lit(Value::Str("a".into())), lit(Value::Str("A".into()))]
        ),
        Ok(Value::Str("a".into()))
    );
}

#[test]
fn coalesce_first_non_null() {
    let args = |v: Vec<Value>| v.into_iter().map(lit).collect::<Vec<_>>();
    assert_eq!(
        call(
            "coalesce",
            vec![lit(Value::Null), lit(Value::Int(2)), poison()]
        ),
        Ok(Value::Int(2))
    );
    assert_eq!(
        call("coalesce", args(vec![Value::Null, Value::Null])),
        Ok(Value::Null)
    );
    let e = call("coalesce", vec![]).unwrap_err();
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
}
