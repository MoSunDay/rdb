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
    assert_eq!(eval_str(&bin(Div, lit(min), lit(-1))), Ok(Value::Int(min)));
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

/// Minimal-but-correct decimal semantics of this batch: exact i128
/// paths for Decimal<->Decimal and Decimal<->Int, coarsened f64 for
/// Double/Str operands (exact paths land in W2.0-exec).
#[test]
fn cmp_decimal_domains() {
    use std::cmp::Ordering::*;
    let d = |m: i128, s: u8| Value::Decimal(m, s);
    assert_eq!(cmp_values(&d(123, 2), &d(124, 2)), Ok(Less));
    assert_eq!(cmp_values(&d(-123, 2), &d(123, 2)), Ok(Less));
    // Cross-scale alignment is exact.
    assert_eq!(cmp_values(&d(123, 2), &d(1230, 3)), Ok(Equal));
    assert_eq!(cmp_values(&d(123, 2), &d(12, 1)), Ok(Greater));
    assert_eq!(cmp_values(&d(123, 2), &d(13, 1)), Ok(Less));
    // Decimal vs Int is exact.
    assert_eq!(cmp_values(&d(250, 2), &Value::Int(2)), Ok(Greater));
    assert_eq!(cmp_values(&d(250, 2), &Value::Int(3)), Ok(Less));
    assert_eq!(cmp_values(&Value::Int(2), &d(200, 2)), Ok(Equal));
    // Double/Str operands coarsen through f64 / a decimal parse.
    assert_eq!(cmp_values(&d(150, 2), &Value::Double(1.5)), Ok(Equal));
    assert_eq!(cmp_values(&Value::Double(1.4), &d(150, 2)), Ok(Less));
    assert_eq!(
        cmp_values(&d(150, 2), &Value::Str("1.50".into())),
        Ok(Equal)
    );
    assert_eq!(
        cmp_values(&Value::Str("1.499".into()), &d(150, 2)),
        Ok(Less)
    );
    assert!(cmp_values(&d(150, 2), &Value::Str("x".into())).is_err());
    // Alignment overflow is a loud error, never a silent order.
    assert!(cmp_values(&Value::Decimal(i128::MAX - 5, 0), &Value::Decimal(0, 38)).is_err());
}

#[test]
fn eval_decimal_arith_and_truthy() {
    use crate::sql::parse::ast::BinOp::*;
    let bin = |op: BinOp, l: Value, r: Value| {
        eval(
            &Expr::BinaryOp {
                left: Box::new(Expr::Lit(l)),
                op,
                right: Box::new(Expr::Lit(r)),
            },
            &Scope,
            &[],
        )
    };
    let d = |m: i128, s: u8| Value::Decimal(m, s);
    // Same-scale add/sub ride i128 directly.
    assert_eq!(bin(Add, d(123, 2), d(456, 2)), Ok(d(579, 2)));
    assert_eq!(bin(Sub, d(123, 2), d(456, 2)), Ok(d(-333, 2)));
    // Cross-scale aligns to the coarser scale; Int lifts to scale 0.
    assert_eq!(bin(Add, d(123, 2), d(4, 1)), Ok(d(163, 2)));
    assert_eq!(bin(Add, d(123, 2), Value::Int(1)), Ok(d(223, 2)));
    // Mul multiplies mantissae and adds scales.
    assert_eq!(bin(Mul, d(123, 2), d(45, 1)), Ok(d(5535, 3)));
    assert_eq!(bin(Mul, d(5, 0), Value::Int(6)), Ok(d(30, 0)));
    // Div scales to the dividend scale + 4 (MySQL
    // div_precision_increment), div-by-zero is NULL.
    assert_eq!(bin(Div, d(100, 0), d(4, 0)), Ok(Value::Decimal(250_000, 4)));
    assert_eq!(bin(Div, d(1, 0), d(4, 0)), Ok(Value::Decimal(2500, 4)));
    assert_eq!(bin(Div, d(1, 2), d(3, 0)), Ok(Value::Decimal(3333, 6)));
    assert_eq!(bin(Div, d(1, 0), d(0, 0)), Ok(Value::Null));
    assert_eq!(bin(Mod, d(7, 0), d(3, 0)), Ok(d(1, 0)));
    assert_eq!(bin(Mod, d(123, 2), Value::Int(1)), Ok(d(23, 2)));
    assert_eq!(bin(Mod, d(7, 0), d(0, 0)), Ok(Value::Null));
    // Mixed with Double: double mode (coarsened).
    assert_eq!(
        bin(Add, d(150, 2), Value::Double(1.0)),
        Ok(Value::Double(2.5))
    );
    // Truthiness is the mantissa.
    assert_eq!(truthy(&d(0, 4)), Ok(false));
    assert_eq!(truthy(&d(1, 38)), Ok(true));
    // Unary neg keeps the scale.
    assert_eq!(
        eval(&Expr::Neg(Box::new(Expr::Lit(d(123, 2)))), &Scope, &[],),
        Ok(d(-123, 2))
    );
    // Overflow refuses rather than wrapping.
    assert!(bin(Add, Value::Decimal(i128::MAX, 0), d(1, 0)).is_err());
    assert!(bin(Mul, Value::Decimal(i128::MAX / 2, 0), d(4, 0)).is_err());
    // Scale sum beyond 38 refuses (mul) instead of mis-storing.
    assert!(bin(Mul, d(1, 20), d(1, 20)).is_err());
}

#[test]
fn coerce_decimal_columns() {
    let dec = |p: u8, s: u8| SqlType::Decimal {
        precision: p,
        scale: s,
    };
    // Int scales up exactly.
    assert_eq!(
        coerce(Value::Int(7), dec(18, 2)),
        Ok(Value::Decimal(700, 2))
    );
    // Strings parse at their own scale, then rescale to the column's.
    assert_eq!(
        coerce(Value::Str("12.345".into()), dec(18, 2)),
        Ok(Value::Decimal(1235, 2)) // rounded half-up from 1234.5
    );
    assert_eq!(
        coerce(Value::Str("-12.345".into()), dec(18, 2)),
        Ok(Value::Decimal(-1235, 2)) // half away from zero
    );
    assert_eq!(
        coerce(Value::Str("12.34".into()), dec(18, 2)),
        Ok(Value::Decimal(1234, 2))
    );
    // Malformed strings are incorrect values (1292 style).
    let err = coerce(Value::Str("12.3.4".into()), dec(18, 2)).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongValue);
    // Cross-scale decimals rescale both directions.
    assert_eq!(
        coerce(Value::Decimal(1234, 3), dec(18, 2)),
        Ok(Value::Decimal(123, 2)) // 1.234 -> 1.23 (round down)
    );
    assert_eq!(
        coerce(Value::Decimal(1235, 3), dec(18, 2)),
        Ok(Value::Decimal(124, 2)) // 1.235 -> 1.24 (half away)
    );
    assert_eq!(
        coerce(Value::Decimal(12, 0), dec(18, 3)),
        Ok(Value::Decimal(12_000, 3))
    );
    // Doubles round half-away at the column scale (through the f64
    // product -- binary-representable inputs only).
    assert_eq!(
        coerce(Value::Double(1.006), dec(18, 2)),
        Ok(Value::Decimal(101, 2))
    );
    assert_eq!(
        coerce(Value::Double(-1.006), dec(18, 2)),
        Ok(Value::Decimal(-101, 2))
    );
    assert_eq!(
        coerce(Value::Double(1.004), dec(18, 2)),
        Ok(Value::Decimal(100, 2))
    );
    // Decimal -> other domains.
    assert_eq!(
        coerce(Value::Decimal(123, 2), SqlType::Int),
        Ok(Value::Int(1))
    );
    assert_eq!(
        coerce(Value::Decimal(-190, 2), SqlType::Int),
        Ok(Value::Int(-1))
    );
    assert_eq!(
        coerce(Value::Decimal(150, 2), SqlType::Double),
        Ok(Value::Double(1.5))
    );
    assert_eq!(
        coerce(Value::Decimal(-123, 2), SqlType::VarChar),
        Ok(Value::Str("-1.23".into()))
    );
    // An Int mantissa beyond the column scale is out of range (loud).
    let err = coerce(Value::Int(i64::MAX), dec(38, 38)).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongValue);
}

// Exact division semantics: the result scale is the dividend scale plus
// four (capped at 38) and the discarded remainder rounds half away from
// zero, so 2/3 -> 0.6667 and -2/3 -> -0.6667 like MySQL.
#[test]
fn eval_division_decimal_scale_and_rounding() {
    use crate::sql::parse::ast::BinOp::*;
    let bin = |op, l, r| {
        eval(
            &Expr::BinaryOp {
                left: Box::new(Expr::Lit(l)),
                op,
                right: Box::new(Expr::Lit(r)),
            },
            &Scope,
            &[],
        )
    };
    let d = |m: i128, s: u8| Value::Decimal(m, s);
    assert_eq!(bin(Div, d(1, 0), d(8, 0)), Ok(d(1250, 4)));
    assert_eq!(bin(Div, d(2, 0), d(3, 0)), Ok(d(6667, 4)));
    assert_eq!(bin(Div, d(-2, 0), d(3, 0)), Ok(d(-6667, 4)));
    assert_eq!(bin(Div, d(1, 0), d(6, 0)), Ok(d(1667, 4)));
    // Scales ride along: dividend scale carries, divisor scale lifts.
    assert_eq!(bin(Div, d(1, 3), d(2, 0)), Ok(d(5000, 7)));
    assert_eq!(bin(Div, d(1, 0), d(25, 2)), Ok(d(40_000, 4)));
    // Past the i128 mantissa budget the value coarsens to Double.
    assert!(matches!(
        bin(Div, Value::Decimal(i128::MAX, 0), d(1, 38)),
        Ok(Value::Double(_))
    ));
}

// ABS keeps the input scale and mirrors the i128::MIN guard the other
// decimal helpers share.
#[test]
fn eval_abs_decimal() {
    let f = |v: Value| {
        eval(
            &Expr::Func {
                name: "abs".into(),
                args: vec![Expr::Lit(v)],
            },
            &Scope,
            &[],
        )
    };
    assert_eq!(f(Value::Decimal(-150, 2)), Ok(Value::Decimal(150, 2)));
    assert_eq!(f(Value::Decimal(150, 2)), Ok(Value::Decimal(150, 2)));
    assert_eq!(f(Value::Decimal(0, 4)), Ok(Value::Decimal(0, 4)));
    assert_eq!(
        f(Value::Decimal(i128::MIN, 2)).unwrap_err().code,
        ErrorCode::WrongValue
    );
}

// The column width is enforced after rounding, so a value that fits
// before rescaling but crosses the precision edge afterwards is loud.
#[test]
fn coerce_decimal_precision_bounds() {
    let dec5 = SqlType::Decimal {
        precision: 5,
        scale: 2,
    };
    assert_eq!(coerce(Value::Int(999), dec5), Ok(Value::Decimal(99900, 2)));
    let over = |v| {
        let e = coerce(v, dec5).unwrap_err();
        (e.code, e.msg.contains("Out of range"))
    };
    assert_eq!(over(Value::Int(1000)), (ErrorCode::WrongValue, true));
    assert_eq!(over(Value::Int(-1000)), (ErrorCode::WrongValue, true));
    // 999.995 rounds up to 1000.00: six digits, past the edge.
    assert_eq!(
        over(Value::Decimal(999_995, 3)),
        (ErrorCode::WrongValue, true)
    );
    // Doubles go through the shortest round-trip text, which avoids the
    // classic 2.675 -> 2.67 f64 artifact.
    let dec2 = SqlType::Decimal {
        precision: 18,
        scale: 2,
    };
    assert_eq!(
        coerce(Value::Double(2.675), dec2),
        Ok(Value::Decimal(268, 2))
    );
}
