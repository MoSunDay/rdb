//! MOD/POW/SQRT/SIGN/GREATEST/LEAST unit tests (the numeric_more half
//! of the family).

use super::*;

#[test]
fn mod_matches_the_operator() {
    // NULL on zero divisor, sign follows the dividend.
    assert_eq!(
        eval("mod", &[Value::Int(7), Value::Int(3)]),
        Some(Ok(Value::Int(1)))
    );
    assert_eq!(
        eval("mod", &[Value::Int(-7), Value::Int(3)]),
        Some(Ok(Value::Int(-1)))
    );
    assert_eq!(
        eval("mod", &[Value::Int(7), Value::Int(0)]),
        Some(Ok(Value::Null))
    );
    assert_eq!(
        eval("mod", &[Value::Decimal(123, 1), Value::Int(2)]),
        Some(Ok(Value::Decimal(3, 1)))
    );
    assert_eq!(
        eval("mod", &[Value::Null, Value::Int(2)]),
        Some(Ok(Value::Null))
    );
}

#[test]
fn pow_sqrt_sign() {
    assert_eq!(
        eval("pow", &[Value::Int(2), Value::Int(10)]),
        Some(Ok(Value::Double(1024.0)))
    );
    assert_eq!(
        eval("power", &[Value::Null, Value::Int(2)]),
        Some(Ok(Value::Null))
    );
    assert_eq!(eval("sqrt", &[Value::Int(9)]), Some(Ok(Value::Double(3.0))));
    // Negative -> NULL (MySQL), not NaN, not an error.
    assert_eq!(eval("sqrt", &[Value::Int(-9)]), Some(Ok(Value::Null)));
    assert_eq!(eval("sign", &[Value::Int(-42)]), Some(Ok(Value::Int(-1))));
    assert_eq!(eval("sign", &[Value::Int(0)]), Some(Ok(Value::Int(0))));
    assert_eq!(
        eval("sign", &[Value::Decimal(150, 2)]),
        Some(Ok(Value::Int(1)))
    );
    assert_eq!(
        eval("sign", &[Value::Double(-0.5)]),
        Some(Ok(Value::Int(-1)))
    );
    assert_eq!(eval("sign", &[Value::Null]), Some(Ok(Value::Null)));
}

#[test]
fn greatest_least_null_and_type_widening() {
    let f = |name: &str, args: &[Value]| eval(name, args).unwrap();
    assert_eq!(
        f("greatest", &[Value::Int(2), Value::Int(10), Value::Int(3)]),
        Ok(Value::Int(10))
    );
    // Any NULL -> NULL.
    assert_eq!(
        f("greatest", &[Value::Int(2), Value::Null]),
        Ok(Value::Null)
    );
    // Widening: Int winner over a decimal arg answers decimal.
    assert_eq!(
        f("greatest", &[Value::Int(5), Value::Decimal(210, 2)]),
        Ok(Value::Decimal(500, 2))
    );
    // A double anywhere doubles the result.
    assert_eq!(
        f("least", &[Value::Int(5), Value::Double(2.5)]),
        Ok(Value::Double(2.5))
    );
    // All-string groups compare byte-wise (case-sensitive, no
    // collation).
    assert_eq!(
        f(
            "greatest",
            &[Value::Str("a".into()), Value::Str("B".into())]
        ),
        Ok(Value::Str("a".into()))
    );
    assert_eq!(
        f("least", &[Value::Str("a".into()), Value::Str("B".into())]),
        Ok(Value::Str("B".into()))
    );
    // Mixed string/number is loud.
    assert!(f("greatest", &[Value::Str("a".into()), Value::Int(1)]).is_err());
}

#[test]
fn greatest_arity_errors() {
    let e = eval("greatest", &[]).unwrap().unwrap_err();
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
}
