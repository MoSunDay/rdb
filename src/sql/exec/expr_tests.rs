use super::*;

struct Scope;
impl ColumnScope for Scope {
    fn resolve(&self, _t: Option<&str>, n: &str) -> Option<usize> {
        match n {
            "a" => Some(0),
            "b" => Some(1),
            "s" => Some(2),
            _ => None,
        }
    }
}

fn eval_str(e: &Expr) -> SqlResult<Value> {
    let row = vec![Value::Int(1), Value::Int(2), Value::Str("hello".into())];
    eval(e, &Scope, &row)
}

#[test]
fn eval_arith_and_compare() {
    use crate::sql::parse::ast::BinOp::*;
    let bin = |op: BinOp, l: Expr, r: Expr| Expr::BinaryOp {
        left: Box::new(l),
        op,
        right: Box::new(r),
    };
    let lit = |i: i64| Expr::Lit(Value::Int(i));
    assert!(matches!(
        eval_str(&bin(Add, lit(2), lit(3))),
        Ok(Value::Int(5))
    ));
    assert!(matches!(
        eval_str(&bin(Div, lit(7), lit(0))),
        Ok(Value::Null)
    ));
    assert!(matches!(
        eval_str(&bin(
            GtEq,
            Expr::Col {
                table: None,
                name: "b".into()
            },
            lit(2)
        )),
        Ok(Value::Bool(true))
    ));
}

#[test]
fn eval_like_patterns() {
    let like = |s: &str, p: &str| like_match(s, p);
    assert!(like("hello", "h%"));
    assert!(like("hello", "%l%"));
    assert!(like("hello", "h_llo"));
    assert!(!like("hello", "h_ll"));
    assert!(like("a%b", r"a\%b"));
}

#[test]
fn eval_null_semantics() {
    // NULL = NULL -> NULL; NULL IN (...) -> NULL
    let e = Expr::BinaryOp {
        left: Box::new(Expr::Lit(Value::Null)),
        op: BinOp::Eq,
        right: Box::new(Expr::Lit(Value::Null)),
    };
    assert!(matches!(eval_str(&e), Ok(Value::Null)));
}

#[test]
fn coerce_types() {
    assert!(matches!(
        coerce(Value::Bool(true), SqlType::Int),
        Ok(Value::Int(1))
    ));
    assert!(coerce(Value::Str("x".into()), SqlType::Int).is_err());
}

#[test]
fn coerce_temporal_strings_and_ints() {
    // 2024-02-29 is day 19782; canonical and compact spellings agree.
    assert_eq!(
        coerce(Value::Str("2024-02-29".into()), SqlType::Date),
        Ok(Value::Date(19_782))
    );
    assert_eq!(
        coerce(Value::Str("20240229".into()), SqlType::Date),
        Ok(Value::Date(19_782))
    );
    assert_eq!(
        coerce(Value::Int(20_240_229), SqlType::Date),
        Ok(Value::Date(19_782))
    );
    assert_eq!(
        coerce(Value::Str("2024-02-29 12:13:14".into()), SqlType::DateTime),
        Ok(Value::DateTime(19_782 * MICROS_PER_DAY + 43_994_000_000))
    );
    assert_eq!(
        coerce(Value::Int(20_240_229_121_314), SqlType::DateTime),
        Ok(Value::DateTime(19_782 * MICROS_PER_DAY + 43_994_000_000))
    );
    // invalid values error loudly, MySQL 1292 style
    let e = coerce(Value::Str("2024-02-30".into()), SqlType::Date).unwrap_err();
    assert_eq!(e.msg, "Incorrect DATE value: '2024-02-30'");
    assert!(coerce(Value::Str("junk".into()), SqlType::DateTime).is_err());
    assert!(coerce(Value::Int(99_999_999), SqlType::Date).is_err());
    // Doubles never reach a temporal column.
    assert!(coerce(Value::Double(20_240_229.0), SqlType::Date).is_err());
}

#[test]
fn coerce_temporal_cross_domain() {
    // Date -> DateTime is midnight; DateTime -> Date truncates.
    assert_eq!(
        coerce(Value::Date(19_782), SqlType::DateTime),
        Ok(Value::DateTime(19_782 * MICROS_PER_DAY))
    );
    assert_eq!(
        coerce(
            Value::DateTime(19_782 * MICROS_PER_DAY + 43_994_000_000),
            SqlType::Date
        ),
        Ok(Value::Date(19_782))
    );
    // Text and compact-integer renderings.
    assert_eq!(
        coerce(Value::Date(19_782), SqlType::VarChar),
        Ok(Value::Str("2024-02-29".into()))
    );
    assert_eq!(
        coerce(
            Value::DateTime(19_782 * MICROS_PER_DAY + 500_000),
            SqlType::VarChar
        ),
        Ok(Value::Str("2024-02-29 00:00:00.500000".into()))
    );
    assert_eq!(
        coerce(Value::Date(19_782), SqlType::Int),
        Ok(Value::Int(20_240_229))
    );
    assert_eq!(
        coerce(
            Value::DateTime(19_782 * MICROS_PER_DAY + 43_994_000_000),
            SqlType::Int
        ),
        Ok(Value::Int(20_240_229_121_314))
    );
    // Blools and bytes never become temporal.
    assert!(coerce(Value::Bool(true), SqlType::Date).is_err());
    assert!(coerce(Value::Date(0), SqlType::Blob).is_err());
}

#[test]
fn cmp_temporal_domains() {
    use std::cmp::Ordering::*;
    let midnight = Value::DateTime(19_782 * MICROS_PER_DAY);
    assert_eq!(cmp_values(&Value::Date(19_782), &midnight).unwrap(), Equal);
    assert_eq!(cmp_values(&midnight, &Value::Date(19_782)).unwrap(), Equal);
    assert_eq!(
        cmp_values(&Value::Date(19_782), &Value::DateTime(0)).unwrap(),
        Greater
    );
    // strings parse in the temporal's domain, both directions
    assert_eq!(
        cmp_values(&Value::Str("2024-03-01".into()), &Value::Date(19_782)).unwrap(),
        Greater
    );
    assert_eq!(
        cmp_values(&Value::Date(19_782), &Value::Str("2024-03-01".into())).unwrap(),
        Less
    );
    // ints compare against the compact form
    assert_eq!(
        cmp_values(&Value::Int(20_240_229), &Value::Date(19_782)).unwrap(),
        Equal
    );
    assert_eq!(
        cmp_values(&Value::Date(19_782), &Value::Int(20_240_228)).unwrap(),
        Greater
    );
    // unparsable strings and doubles stay loud
    let e = cmp_values(&Value::Str("garbage".into()), &Value::Date(1)).unwrap_err();
    assert_eq!(e.msg, "Incorrect DATE value: 'garbage'");
    assert!(cmp_values(&Value::Double(1.0), &Value::Date(1)).is_err());
}

#[test]
fn temporal_arith_errors_loudly() {
    let e = eval_str(&Expr::BinaryOp {
        left: Box::new(Expr::Lit(Value::Date(1))),
        op: BinOp::Add,
        right: Box::new(Expr::Lit(Value::Int(1))),
    })
    .unwrap_err();
    assert_eq!(e.msg, "DATE/DATETIME values do not support arithmetic");
}

#[test]
fn clock_functions_smoke() {
    // Kind only -- the value moves with the wall clock. (Function
    // names arrive lowercased from the translator.)
    assert!(matches!(eval_func("now", &[]), Ok(Value::DateTime(_))));
    assert!(matches!(
        eval_func("current_timestamp", &[Value::Int(6)]),
        Ok(Value::DateTime(_))
    ));
    assert!(matches!(
        eval_func("localtimestamp", &[]),
        Ok(Value::DateTime(_))
    ));
    assert!(matches!(eval_func("curdate", &[]), Ok(Value::Date(_))));
    assert!(matches!(
        eval_func("current_date", &[Value::Int(0)]),
        Ok(Value::Date(_))
    ));
    assert!(eval_func("now", &[Value::Int(1), Value::Int(2)]).is_err());
}

/// NOT routes through three-valued logic: NOT NULL stays unknown
/// (it used to collapse NULL to FALSE and report TRUE).
#[test]
fn not_null_is_null() {
    let not = |e: Expr| Expr::Not(Box::new(e));
    assert!(matches!(
        eval_str(&not(Expr::Lit(Value::Null))),
        Ok(Value::Null)
    ));
    assert_eq!(
        eval_str(&not(Expr::Lit(Value::Bool(true)))),
        Ok(Value::Bool(false))
    );
    assert_eq!(
        eval_str(&not(Expr::Lit(Value::Int(0)))),
        Ok(Value::Bool(true))
    );
}

/// IN compares every non-NULL member before a NULL member can turn the
/// verdict unknown (three-valued IN: equality short-circuits first).
#[test]
fn in_null_semantics() {
    let in_list = |v: Value, items: Vec<Value>, negated: bool| Expr::InList {
        expr: Box::new(Expr::Lit(v)),
        list: items.into_iter().map(Expr::Lit).collect(),
        negated,
    };
    let null = || Value::Null;
    assert_eq!(
        eval_str(&in_list(Value::Int(1), vec![null(), Value::Int(1)], false)),
        Ok(Value::Bool(true))
    );
    // No member compared equal, so the NULL member keeps the verdict
    // unknown (MySQL: SELECT 2 IN (NULL, 1) -> NULL).
    assert!(matches!(
        eval_str(&in_list(Value::Int(2), vec![null(), Value::Int(1)], false)),
        Ok(Value::Null)
    ));
    assert!(matches!(
        eval_str(&in_list(Value::Int(2), vec![null(), Value::Int(3)], false)),
        Ok(Value::Null)
    ));
    assert!(matches!(
        eval_str(&in_list(Value::Int(1), vec![null(), Value::Int(2)], true)),
        Ok(Value::Null)
    ));
    // A matching member still wins over the NULL member, even under NOT.
    assert_eq!(
        eval_str(&in_list(Value::Int(1), vec![null(), Value::Int(1)], true)),
        Ok(Value::Bool(false))
    );
    assert!(matches!(
        eval_str(&in_list(Value::Int(2), vec![null(), Value::Int(1)], true)),
        Ok(Value::Null)
    ));
    assert!(matches!(
        eval_str(&in_list(Value::Int(3), vec![null(), Value::Int(2)], true)),
        Ok(Value::Null)
    ));
    // NULL on the left stays unknown whatever the members are.
    assert!(matches!(
        eval_str(&in_list(null(), vec![Value::Int(1)], false)),
        Ok(Value::Null)
    ));
}

/// MySQL LENGTH() counts BYTES, CHAR_LENGTH() counts characters.
#[test]
fn length_counts_bytes_char_length_counts_chars() {
    // 'héllo': 5 chars, 6 bytes (é is two bytes in utf-8).
    assert_eq!(
        eval_func("length", &[Value::Str("h\u{e9}llo".into())]),
        Ok(Value::Int(6))
    );
    assert_eq!(
        eval_func("char_length", &[Value::Str("h\u{e9}llo".into())]),
        Ok(Value::Int(5))
    );
    assert!(matches!(
        eval_func("length", &[Value::Null]),
        Ok(Value::Null)
    ));
}

/// Integer div/mod wrap like Add/Sub/Mul: MIN / -1 must not panic
/// (divide-by-zero still evaluates to NULL).
#[test]
fn eval_int_div_mod_wrap_extremes() {
    use crate::sql::parse::ast::BinOp::*;
    let bin = |op: BinOp, l: Expr, r: Expr| Expr::BinaryOp {
        left: Box::new(l),
        op,
        right: Box::new(r),
    };
    let lit = |i: i64| Expr::Lit(Value::Int(i));
    let min = i64::MIN;
    assert_eq!(
        eval_str(&bin(Div, lit(min), lit(-1))),
        Ok(Value::Int(min))
    );
    assert_eq!(eval_str(&bin(Mod, lit(min), lit(-1))), Ok(Value::Int(0)));
    // divide / modulo by zero stays NULL
    assert!(matches!(
        eval_str(&bin(Div, lit(7), lit(0))),
        Ok(Value::Null)
    ));
    assert!(matches!(
        eval_str(&bin(Mod, lit(7), lit(0))),
        Ok(Value::Null)
    ));
    // ordinary division and modulo are unchanged
    assert!(matches!(
        eval_str(&bin(Div, lit(7), lit(2))),
        Ok(Value::Int(3))
    ));
    assert!(matches!(
        eval_str(&bin(Mod, lit(7), lit(2))),
        Ok(Value::Int(1))
    ));
}
