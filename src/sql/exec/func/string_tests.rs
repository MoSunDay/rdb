//! String-family unit tests (moved from exec/expr_tests.rs when the
//! function evaluation split into families; behavior unchanged).

use super::*;

#[test]
fn length_counts_bytes_char_length_counts_chars() {
    // MySQL: LENGTH() counts bytes, CHAR_LENGTH() counts characters.
    assert_eq!(
        eval("length", &[Value::Str("h\u{e9}llo".into())]),
        Some(Ok(Value::Int(6)))
    );
    assert_eq!(
        eval("char_length", &[Value::Str("h\u{e9}llo".into())]),
        Some(Ok(Value::Int(5)))
    );
    assert_eq!(eval("length", &[Value::Null]), Some(Ok(Value::Null)));
    assert_eq!(
        eval("length", &[Value::Bytes(vec![1, 2, 3])]),
        Some(Ok(Value::Int(3)))
    );
    // Non-string domains stay loud, not zero.
    assert!(eval("length", &[Value::Int(3)]).unwrap().is_err());
}

#[test]
fn upper_lower_fold_ascii_and_beyond() {
    assert_eq!(
        eval("upper", &[Value::Str("hé".into())]),
        Some(Ok(Value::Str("HÉ".into())))
    );
    assert_eq!(
        eval("lower", &[Value::Str("HÉ".into())]),
        Some(Ok(Value::Str("hé".into())))
    );
    assert_eq!(eval("upper", &[Value::Null]), Some(Ok(Value::Null)));
    assert!(eval("lower", &[Value::Int(1)]).unwrap().is_err());
    // Bad arity is the family's error (the parse-time signature table
    // normally catches it first).
    assert!(eval("upper", &[]).unwrap().is_err());
}

// Byte-oriented, case-sensitive, unanchored; either side NULL is NULL
// and a bad pattern errors loudly instead of mismatching.
#[test]
fn regexp_matches_bytes_case_sensitively() {
    let s = |v: &str| Value::Str(v.into());
    assert_eq!(regexp_match(&s("hello"), &s("^h.*o$")), Ok(Value::Int(1)));
    assert_eq!(regexp_match(&s("Hello"), &s("^h")), Ok(Value::Int(0)));
    assert_eq!(regexp_match(&s("abc"), &s("b")), Ok(Value::Int(1)));
    assert_eq!(regexp_match(&Value::Null, &s("a")), Ok(Value::Null));
    assert_eq!(regexp_match(&s("a"), &Value::Null), Ok(Value::Null));
    assert_eq!(
        regexp_match(&Value::Bytes(b"ab".to_vec()), &s("a")),
        Ok(Value::Int(1))
    );
    let e = regexp_match(&s("a"), &s("(unclosed")).unwrap_err();
    assert!(e.msg.contains("Incorrect REGEXP"), "{e}");
    assert!(regexp_match(&Value::Int(1), &s("a")).is_err());
}

// ---- part B: CONCAT family, LEFT/RIGHT, REPEAT/REVERSE, HEX/UNHEX ----

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

#[test]
fn concat_coerces_and_propagates_null() {
    assert_eq!(
        eval("concat", &[s("a"), Value::Int(1), Value::Double(2.5)]),
        Some(Ok(s("a12.5")))
    );
    // Decimal keeps its exact scale in the text form.
    assert_eq!(
        eval("concat", &[Value::Decimal(200, 2), s("x")]),
        Some(Ok(s("2.00x")))
    );
    assert_eq!(
        eval("concat", &[s("a"), Value::Null, s("b")]),
        Some(Ok(Value::Null))
    );
    assert!(eval("concat", &[])
        .unwrap()
        .unwrap_err()
        .msg
        .contains("Incorrect parameter count"));
    // Binary-safe: blobs join when valid utf8.
    assert_eq!(
        eval("concat", &[Value::Bytes(b"ab".to_vec()), s("c")]),
        Some(Ok(s("abc")))
    );
}

#[test]
fn concat_ws_skips_nulls_but_null_sep_is_null() {
    assert_eq!(
        eval("concat_ws", &[s("-"), s("a"), Value::Null, s("b")]),
        Some(Ok(s("a-b")))
    );
    assert_eq!(
        eval("concat_ws", &[Value::Null, s("a"), s("b")]),
        Some(Ok(Value::Null))
    );
    // All-NULL tail is the empty string, not NULL.
    assert_eq!(
        eval("concat_ws", &[s(","), Value::Null, Value::Null]),
        Some(Ok(s("")))
    );
    // Empty strings are real arguments: the separators around them
    // stay (the old !out.is_empty() gate dropped them).
    assert_eq!(
        eval("concat_ws", &[s(","), s(""), s("b")]),
        Some(Ok(s(",b")))
    );
    assert_eq!(
        eval("concat_ws", &[s(","), s("a"), s(""), s("b")]),
        Some(Ok(s("a,,b")))
    );
    assert!(eval("concat_ws", &[s(",")]).unwrap().is_err());
}

#[test]
fn left_right_count_characters() {
    assert_eq!(
        eval("left", &[s("hello"), Value::Int(2)]),
        Some(Ok(s("he")))
    );
    assert_eq!(
        eval("right", &[s("hello"), Value::Int(2)]),
        Some(Ok(s("lo")))
    );
    // UTF-8: characters, not bytes.
    assert_eq!(
        eval("left", &[s("hé价llo"), Value::Int(3)]),
        Some(Ok(s("hé价")))
    );
    assert_eq!(eval("right", &[s("hello"), Value::Int(0)]), Some(Ok(s(""))));
    assert_eq!(eval("left", &[s("ab"), Value::Null]), Some(Ok(Value::Null)));
}

#[test]
fn repeat_and_reverse() {
    assert_eq!(
        eval("repeat", &[s("ab"), Value::Int(3)]),
        Some(Ok(s("ababab")))
    );
    // 0 or negative -> ''.
    assert_eq!(eval("repeat", &[s("ab"), Value::Int(0)]), Some(Ok(s(""))));
    assert_eq!(eval("repeat", &[s("ab"), Value::Int(-1)]), Some(Ok(s(""))));
    assert_eq!(eval("reverse", &[s("hé价")]), Some(Ok(s("价éh"))));
}

#[test]
fn repeat_caps_the_result_at_the_packet_maximum() {
    // The cap is the result BYTE length: exactly 1 << 24 bytes is the
    // last legal size (MySQL's max wire packet), one byte more is NULL.
    let cap = 1i64 << 24;
    let ok = eval("repeat", &[s("x"), Value::Int(cap)]).unwrap().unwrap();
    assert_eq!(ok, Value::Str("x".repeat(cap as usize)));
    assert_eq!(
        eval("repeat", &[s("x"), Value::Int(cap + 1)]),
        Some(Ok(Value::Null))
    );
    // Multi-byte inputs count bytes, not chars: half the chars of a
    // 2-byte char already pass the cap.
    assert_eq!(
        eval("repeat", &[s("é"), Value::Int((cap / 2) + 1)]),
        Some(Ok(Value::Null))
    );
    // Far-past requests never allocate first.
    assert_eq!(
        eval("repeat", &[s("x"), Value::Int(1 << 30)]),
        Some(Ok(Value::Null))
    );
}

#[test]
fn hex_unhex_roundtrip() {
    assert_eq!(eval("hex", &[s("abc")]), Some(Ok(s("616263"))));
    // MySQL prints uppercase hex letters.
    assert_eq!(
        eval("hex", &[Value::Bytes(vec![0x0a, 0xff])]),
        Some(Ok(s("0AFF")))
    );
    assert_eq!(
        eval("hex", &[Value::Int(255)]),
        Some(Ok(s("00000000000000FF")))
    );
    assert_eq!(
        eval("hex", &[Value::Int(-1)]),
        Some(Ok(s("FFFFFFFFFFFFFFFF")))
    );
    assert_eq!(eval("hex", &[Value::Null]), Some(Ok(Value::Null)));
    assert_eq!(
        eval("unhex", &[s("616263")]),
        Some(Ok(Value::Bytes(b"abc".to_vec())))
    );
    // Odd length / bad digits -> NULL, not an error.
    assert_eq!(eval("unhex", &[s("4")]), Some(Ok(Value::Null)));
    assert_eq!(eval("unhex", &[s("zz")]), Some(Ok(Value::Null)));
    assert_eq!(eval("unhex", &[Value::Null]), Some(Ok(Value::Null)));
}

#[test]
fn value_text_renders_every_domain() {
    use crate::sql::temporal;
    assert_eq!(value_text(&Value::Bool(true)), Ok("1".to_string()));
    assert_eq!(value_text(&Value::Date(0)), Ok("1970-01-01".to_string()));
    let midnight = temporal::days_from_civil(2024, 1, 2).unwrap() * temporal::MICROS_PER_DAY;
    assert_eq!(
        value_text(&Value::DateTime(midnight)),
        Ok("2024-01-02 00:00:00".to_string())
    );
}
