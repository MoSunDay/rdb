//! SUBSTRING/LPAD/RPAD/LOCATE/INSTR/REPLACE/TRIM unit tests (the
//! string_more half of the family).

use super::*;

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

#[test]
fn substring_one_based_with_negative_pos() {
    let f = |args: &[Value]| eval("substring", args).unwrap();
    assert_eq!(f(&[s("Quadratically"), Value::Int(5)]), Ok(s("ratically")));
    assert_eq!(
        f(&[s("Quadratically"), Value::Int(5), Value::Int(6)]),
        Ok(s("ratica"))
    );
    // Negative pos counts from the end.
    assert_eq!(f(&[s("Quadratically"), Value::Int(-5)]), Ok(s("cally")));
    assert_eq!(
        f(&[s("Quadratically"), Value::Int(-5), Value::Int(3)]),
        Ok(s("cal"))
    );
    // pos 0 / past the end / len <= 0 -> ''.
    assert_eq!(f(&[s("abc"), Value::Int(0)]), Ok(s("")));
    assert_eq!(f(&[s("abc"), Value::Int(9)]), Ok(s("")));
    assert_eq!(f(&[s("abc"), Value::Int(1), Value::Int(0)]), Ok(s("")));
    // Characters, not bytes.
    assert_eq!(
        f(&[s("hé价llo"), Value::Int(2), Value::Int(2)]),
        Ok(s("é价"))
    );
    assert_eq!(f(&[s("abc"), Value::Null]), Ok(Value::Null));
    assert!(eval("substring", &[s("a")]).unwrap().is_err());
}

#[test]
fn lpad_rpad_truncate_repeat_and_null_pad() {
    let f = |name: &str, args: &[Value]| eval(name, args).unwrap();
    // len < s truncates (head kept for both).
    assert_eq!(f("lpad", &[s("hello"), Value::Int(2), s("?")]), Ok(s("he")));
    assert_eq!(f("rpad", &[s("hello"), Value::Int(2), s("?")]), Ok(s("he")));
    // Pad repeats when shorter than needed.
    assert_eq!(
        f("lpad", &[s("hi"), Value::Int(7), s("?.")]),
        Ok(s("?.?.?hi"))
    );
    assert_eq!(
        f("rpad", &[s("hi"), Value::Int(7), s("?.")]),
        Ok(s("hi?.?.?"))
    );
    // NULL pad -> NULL (MySQL), NULL s -> NULL.
    assert_eq!(
        f("lpad", &[s("hi"), Value::Int(7), Value::Null]),
        Ok(Value::Null)
    );
    assert_eq!(
        f("rpad", &[Value::Null, Value::Int(7), s("?")]),
        Ok(Value::Null)
    );
    // Same length is an identity.
    assert_eq!(f("lpad", &[s("hi"), Value::Int(2), s("?")]), Ok(s("hi")));
    assert!(eval("lpad", &[s("a"), Value::Int(2)]).unwrap().is_err());
}

#[test]
fn locate_and_instr_are_one_search() {
    let f = |name: &str, args: &[Value]| eval(name, args).unwrap();
    assert_eq!(f("locate", &[s("bar"), s("foarbar")]), Ok(Value::Int(5)));
    assert_eq!(f("locate", &[s("x"), s("abc")]), Ok(Value::Int(0)));
    // Start position: only whole matches at/after it count.
    assert_eq!(
        f("locate", &[s("bar"), s("barbar"), Value::Int(4)]),
        Ok(Value::Int(4))
    );
    assert_eq!(
        f("locate", &[s("bar"), s("barbar"), Value::Int(5)]),
        Ok(Value::Int(0))
    );
    // Before-position < 1 never matches.
    assert_eq!(
        f("locate", &[s("a"), s("abc"), Value::Int(0)]),
        Ok(Value::Int(0))
    );
    // INSTR flips the arguments.
    assert_eq!(f("instr", &[s("foarbar"), s("bar")]), Ok(Value::Int(5)));
    assert_eq!(f("locate", &[Value::Null, s("abc")]), Ok(Value::Null));
    assert_eq!(f("instr", &[s("abc"), Value::Null]), Ok(Value::Null));
    // Characters, not bytes.
    assert_eq!(f("locate", &[s("价"), s("hé价")]), Ok(Value::Int(3)));
}

#[test]
fn replace_and_empty_from() {
    let f = |args: &[Value]| eval("replace", args).unwrap();
    assert_eq!(
        f(&[s("www.mysql.com"), s("mysql"), s("rdb")]),
        Ok(s("www.rdb.com"))
    );
    // Empty `from` leaves the string unchanged.
    assert_eq!(f(&[s("abc"), s(""), s("x")]), Ok(s("abc")));
    assert_eq!(f(&[s("aaa"), s("a"), s("")]), Ok(s("")));
    assert_eq!(f(&[s("a"), Value::Null, s("b")]), Ok(Value::Null));
}

#[test]
fn trim_spaces_and_remstr_forms() {
    let f = |args: &[Value]| eval("trim", args).unwrap();
    assert_eq!(f(&[s("  bar  ")]), Ok(s("bar")));
    assert_eq!(f(&[Value::Null]), Ok(Value::Null));
    // The translated 3-arg forms (mode literal from TRIM(.. FROM ..)).
    assert_eq!(f(&[s("xxxbarxxx"), s("x"), s("BOTH")]), Ok(s("bar")));
    assert_eq!(f(&[s("xxxbarxxx"), s("x"), s("LEADING")]), Ok(s("barxxx")));
    assert_eq!(f(&[s("xxxbarxxx"), s("x"), s("TRAILING")]), Ok(s("xxxbar")));
    // Multi-char remstr strips whole units, repeatedly.
    assert_eq!(f(&[s("ababfoo"), s("ab"), s("LEADING")]), Ok(s("foo")));
    // A remstr of spaces behaves like the plain form.
    assert_eq!(f(&[s("  bar"), s(" "), s("LEADING")]), Ok(s("bar")));
    assert!(eval("trim", &[s("a"), s("b")]).unwrap().is_err());
    assert!(eval("trim", &[s("a"), s("b"), s("NOPE")]).unwrap().is_err());
}
