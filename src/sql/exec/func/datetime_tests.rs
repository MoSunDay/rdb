//! Clock-family unit tests (moved from exec/expr_tests.rs; kind-only
//! assertions -- the values move with the wall clock).

use super::*;

#[test]
fn clock_functions_smoke() {
    // Kind only -- the value moves with the wall clock. (Function
    // names arrive lowercased from the translator.)
    assert!(matches!(eval("now", &[]), Some(Ok(Value::DateTime(_)))));
    assert!(matches!(
        eval("current_timestamp", &[Value::Int(6)]),
        Some(Ok(Value::DateTime(_)))
    ));
    assert!(matches!(
        eval("localtimestamp", &[]),
        Some(Ok(Value::DateTime(_)))
    ));
    assert!(matches!(eval("sysdate", &[]), Some(Ok(Value::DateTime(_)))));
    assert!(matches!(eval("curdate", &[]), Some(Ok(Value::Date(_)))));
    assert!(matches!(
        eval("current_date", &[Value::Int(0)]),
        Some(Ok(Value::Date(_)))
    ));
    // fsp beyond the one allowed argument is a wrong parameter count.
    let e = eval("now", &[Value::Int(1), Value::Int(2)])
        .unwrap()
        .unwrap_err();
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
    assert!(eval("utc_timestamp", &[]).is_none(), "not this family");
}

// ---- part B: CURTIME family + extraction ----

#[test]
fn curtime_renders_hms_text() {
    for name in ["curtime", "current_time"] {
        let Some(Ok(Value::Str(t))) = eval(name, &[]) else {
            panic!("{name} -> Str");
        };
        // HH:MM:SS shape, 00..23 hours.
        assert_eq!(t.len(), 8, "{t}");
        assert_eq!(&t[2..3], ":");
        assert!(&t[0..2] <= "23", "{t}");
    }
    assert!(eval("curtime", &[Value::Int(1), Value::Int(2)])
        .unwrap()
        .is_err());
}

#[test]
fn date_extracts_date_part() {
    let f = |args: &[Value]| eval("date", args).unwrap();
    assert_eq!(
        f(&[Value::Str("2024-01-02 03:04:05".into())]),
        Ok(Value::Date(days(2024, 1, 2)))
    );
    // Bare date string and the compact int forms.
    assert_eq!(
        f(&[Value::Str("20240102".into())]),
        Ok(Value::Date(days(2024, 1, 2)))
    );
    assert_eq!(
        f(&[Value::Int(20240102)]),
        Ok(Value::Date(days(2024, 1, 2)))
    );
    assert_eq!(
        f(&[Value::Int(20240102030405)]),
        Ok(Value::Date(days(2024, 1, 2)))
    );
    // Date/DateTime inputs.
    assert_eq!(
        f(&[Value::Date(days(2024, 1, 2))]),
        Ok(Value::Date(days(2024, 1, 2)))
    );
    // Invalid spellings are NULL (permissive read path).
    assert_eq!(f(&[Value::Str("not-a-date".into())]), Ok(Value::Null));
    assert_eq!(f(&[Value::Int(1)]), Ok(Value::Null));
    assert_eq!(f(&[Value::Null]), Ok(Value::Null));
}

#[test]
fn year_month_day_extraction() {
    let f = |name: &str| {
        eval(
            name,
            std::slice::from_ref(&Value::Str("2024-03-05 06:07:08".into())),
        )
    };
    assert_eq!(f("year"), Some(Ok(Value::Int(2024))));
    assert_eq!(f("month"), Some(Ok(Value::Int(3))));
    assert_eq!(f("day"), Some(Ok(Value::Int(5))));
    assert_eq!(f("dayofmonth"), Some(Ok(Value::Int(5))));
    // Hour parts come from the time-of-day.
    assert_eq!(f("hour"), Some(Ok(Value::Int(6))));
    assert_eq!(f("minute"), Some(Ok(Value::Int(7))));
    assert_eq!(f("second"), Some(Ok(Value::Int(8))));
    // A bare date is midnight.
    assert_eq!(
        eval("hour", &[Value::Date(days(2024, 1, 2))]),
        Some(Ok(Value::Int(0)))
    );
    // NULL / invalid propagate as NULL.
    assert_eq!(eval("year", &[Value::Null]), Some(Ok(Value::Null)));
    assert_eq!(
        eval("month", &[Value::Str("junk".into())]),
        Some(Ok(Value::Null))
    );
    assert!(eval("year", &[]).unwrap().is_err());
}

fn days(y: i64, m: u32, d: u32) -> i64 {
    crate::sql::temporal::days_from_civil(y, m, d).expect("valid civil date")
}
