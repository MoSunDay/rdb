//! Numeric-family unit tests (abs moved from exec/expr_tests.rs) plus
//! the 64-bit-unsigned bit-operator semantics used by `eval_binop`.

use super::*;

#[test]
fn abs_keeps_scale_and_guards_min() {
    assert_eq!(
        eval("abs", &[Value::Decimal(-150, 2)]),
        Some(Ok(Value::Decimal(150, 2)))
    );
    assert_eq!(
        eval("abs", &[Value::Decimal(150, 2)]),
        Some(Ok(Value::Decimal(150, 2)))
    );
    assert_eq!(
        eval("abs", &[Value::Decimal(0, 4)]),
        Some(Ok(Value::Decimal(0, 4)))
    );
    assert_eq!(eval("abs", &[Value::Int(-5)]), Some(Ok(Value::Int(5))));
    assert_eq!(eval("abs", &[Value::Int(5)]), Some(Ok(Value::Int(5))));
    // i64::MIN has no positive peer: loud 1690-style error, the old
    // code silently wrapped back to MIN.
    let e = eval("abs", &[Value::Int(i64::MIN)]).unwrap().unwrap_err();
    assert_eq!(e.code, crate::sql::parse::error::ErrorCode::WrongValue);
    assert_eq!(
        e.msg,
        "BIGINT value is out of range in 'ABS(-9223372036854775808)'"
    );
    assert_eq!(
        eval("abs", &[Value::Double(-2.5)]),
        Some(Ok(Value::Double(2.5)))
    );
    assert_eq!(eval("abs", &[Value::Null]), Some(Ok(Value::Null)));
    // i128::MIN has no positive peer: loud, never wrapped.
    assert_eq!(
        eval("abs", &[Value::Decimal(i128::MIN, 2)])
            .unwrap()
            .unwrap_err()
            .code,
        crate::sql::parse::error::ErrorCode::WrongValue
    );
    assert!(eval("abs", &[Value::Str("x".into())]).unwrap().is_err());
}

fn bit(op: BinOp, l: Value, r: Value) -> SqlResult<Value> {
    eval_bitop(&op, &l, &r)
}

// Negative Ints reinterpret as two's-complement u64, exactly like
// MySQL: -1 & 255 = 255, -1 >> 1 keeps every high bit.
#[test]
fn bitops_use_u64_two_complement_semantics() {
    use crate::sql::parse::ast::BinOp::*;
    assert_eq!(
        bit(BitAnd, Value::Int(-1), Value::Int(255)),
        Ok(Value::Int(255))
    );
    assert_eq!(bit(BitOr, Value::Int(0), Value::Int(5)), Ok(Value::Int(5)));
    assert_eq!(bit(BitXor, Value::Int(5), Value::Int(3)), Ok(Value::Int(6)));
    assert_eq!(
        bit(Shl, Value::Int(1), Value::Int(10)),
        Ok(Value::Int(1024))
    );
    // Logical shift on the u64 view: -1 >> 1 is i64::MAX (MySQL agrees).
    assert_eq!(
        bit(Shr, Value::Int(-1), Value::Int(1)),
        Ok(Value::Int(i64::MAX))
    );
    assert_eq!(
        bit(Shr, Value::Int(1024), Value::Int(3)),
        Ok(Value::Int(128))
    );
    // Shifts past the width shift everything out (no 6-bit masking).
    assert_eq!(bit(Shl, Value::Int(1), Value::Int(64)), Ok(Value::Int(0)));
    assert_eq!(bit(Shr, Value::Int(-1), Value::Int(100)), Ok(Value::Int(0)));
    // The full u64 range survives round-trip: 1 << 63 is i64::MIN.
    assert_eq!(
        bit(Shl, Value::Int(1), Value::Int(63)),
        Ok(Value::Int(i64::MIN))
    );
    // NULL propagates; non-Int domains are loud.
    assert_eq!(bit(BitAnd, Value::Null, Value::Int(1)), Ok(Value::Null));
    assert!(bit(BitAnd, Value::Double(1.0), Value::Int(1)).is_err());
}

// ---- part B: ROUND / CEIL / FLOOR / TRUNCATE ----

#[test]
fn round_half_away_from_zero() {
    // Decimal is exact: 2.005 at scale 2 rounds away.
    assert_eq!(
        eval("round", &[Value::Decimal(2005, 3), Value::Int(2)]),
        Some(Ok(Value::Decimal(201, 2)))
    );
    assert_eq!(
        eval("round", &[Value::Decimal(25, 1)]),
        Some(Ok(Value::Decimal(3, 0)))
    );
    assert_eq!(
        eval("round", &[Value::Decimal(-25, 1)]),
        Some(Ok(Value::Decimal(-3, 0)))
    );
    // d beyond the scale is an identity.
    assert_eq!(
        eval("round", &[Value::Decimal(200, 2), Value::Int(5)]),
        Some(Ok(Value::Decimal(200, 2)))
    );
    // Negative d scales the integer part down, scale-0 result.
    assert_eq!(
        eval("round", &[Value::Decimal(1500, 2), Value::Int(-1)]),
        Some(Ok(Value::Decimal(20, 0)))
    );
    // 15.00 -> 20 (tie rounds away).
    assert_eq!(
        eval("round", &[Value::Decimal(1500, 2), Value::Int(-1)]),
        Some(Ok(Value::Decimal(20, 0)))
    );
    assert_eq!(
        eval("round", &[Value::Decimal(-1500, 2), Value::Int(-1)]),
        Some(Ok(Value::Decimal(-20, 0)))
    );
}

#[test]
fn round_int_and_double_domains() {
    // Int stays Int; d >= 0 is an identity.
    assert_eq!(
        eval("round", &[Value::Int(125), Value::Int(2)]),
        Some(Ok(Value::Int(125)))
    );
    assert_eq!(
        eval("round", &[Value::Int(125), Value::Int(-1)]),
        Some(Ok(Value::Int(130)))
    );
    assert_eq!(
        eval("round", &[Value::Int(-125), Value::Int(-1)]),
        Some(Ok(Value::Int(-130)))
    );
    // Doubles ride f64 rounding: ROUND(-2.5) = -3.
    assert_eq!(
        eval("round", &[Value::Double(-2.5)]),
        Some(Ok(Value::Double(-3.0)))
    );
    assert_eq!(
        eval("round", &[Value::Double(2.5)]),
        Some(Ok(Value::Double(3.0)))
    );
    assert_eq!(
        eval("round", &[Value::Double(1.234), Value::Int(2)]),
        Some(Ok(Value::Double(1.23)))
    );
    assert_eq!(
        eval("round", &[Value::Double(1234.0), Value::Int(-2)]),
        Some(Ok(Value::Double(1200.0)))
    );
    assert_eq!(eval("round", &[Value::Null]), Some(Ok(Value::Null)));
    assert!(eval("round", &[Value::Str("1".into())]).unwrap().is_err());
    let e = eval("round", &[Value::Int(1), Value::Int(2), Value::Int(3)])
        .unwrap()
        .unwrap_err();
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
}

#[test]
fn ceil_floor_mirror_the_input_domain() {
    let f = |name: &str, args: &[Value]| eval(name, args).unwrap();
    // Exact decimal: scale-0 decimal result.
    assert_eq!(
        f("ceil", &[Value::Decimal(123, 2)]),
        Ok(Value::Decimal(2, 0))
    );
    assert_eq!(
        f("ceil", &[Value::Decimal(-123, 2)]),
        Ok(Value::Decimal(-1, 0))
    );
    assert_eq!(
        f("floor", &[Value::Decimal(-123, 2)]),
        Ok(Value::Decimal(-2, 0))
    );
    assert_eq!(
        f("ceiling", &[Value::Decimal(200, 2)]),
        Ok(Value::Decimal(2, 0))
    );
    // Int is an identity, double rides f64.
    assert_eq!(f("ceil", &[Value::Int(-5)]), Ok(Value::Int(-5)));
    assert_eq!(f("floor", &[Value::Double(1.7)]), Ok(Value::Double(1.0)));
    assert_eq!(f("ceil", &[Value::Double(-1.2)]), Ok(Value::Double(-1.0)));
    assert_eq!(f("floor", &[Value::Null]), Ok(Value::Null));
}

#[test]
fn truncate_cuts_toward_zero() {
    let f = |args: &[Value]| eval("truncate", args).unwrap();
    assert_eq!(
        f(&[Value::Decimal(1223, 2), Value::Int(1)]),
        Ok(Value::Decimal(122, 1))
    );
    assert_eq!(
        f(&[Value::Decimal(-1223, 2), Value::Int(1)]),
        Ok(Value::Decimal(-122, 1))
    );
    // d beyond the scale is an identity; negative d cuts integer digits.
    assert_eq!(
        f(&[Value::Decimal(200, 2), Value::Int(5)]),
        Ok(Value::Decimal(200, 2))
    );
    assert_eq!(
        f(&[Value::Decimal(1299, 2), Value::Int(-2)]),
        Ok(Value::Decimal(0, 0)) // 12.99 -> 0 hundreds
    );
    assert_eq!(
        f(&[Value::Decimal(1299, 0), Value::Int(-2)]),
        Ok(Value::Decimal(1200, 0)) // 1299 -> 1200, scale 0
    );
    assert_eq!(f(&[Value::Int(129), Value::Int(-1)]), Ok(Value::Int(120)));
    assert_eq!(
        f(&[Value::Double(-1.999), Value::Int(1)]),
        Ok(Value::Double(-1.9))
    );
    assert_eq!(f(&[Value::Null, Value::Int(1)]), Ok(Value::Null));
}

#[test]
fn round_truncate_null_scale_is_null() {
    // MySQL: a NULL digits argument is NULL, never a loud coercion
    // error out of the count slot.
    assert_eq!(
        eval("round", &[Value::Double(2.5), Value::Null]).unwrap(),
        Ok(Value::Null)
    );
    assert_eq!(
        eval("round", &[Value::Decimal(123, 2), Value::Null]).unwrap(),
        Ok(Value::Null)
    );
    assert_eq!(
        eval("truncate", &[Value::Double(2.5), Value::Null]).unwrap(),
        Ok(Value::Null)
    );
    assert_eq!(
        eval("truncate", &[Value::Decimal(1223, 2), Value::Null]).unwrap(),
        Ok(Value::Null)
    );
}
