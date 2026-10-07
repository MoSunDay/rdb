//! DATE_ADD/SUB, DATEDIFF, DATE_FORMAT and the UNIX_TIMESTAMP pair.

use super::*;

fn days(y: i64, m: u32, d: u32) -> i64 {
    crate::sql::temporal::days_from_civil(y, m, d).expect("valid civil date")
}

fn dt(y: i64, m: u32, d: u32, h: i64, mi: i64, s: i64) -> Value {
    Value::DateTime(
        days(y, m, d) * crate::sql::temporal::MICROS_PER_DAY + (h * 3600 + mi * 60 + s) * 1_000_000,
    )
}

fn add(name: &str, d: Value, n: i64, unit: &str) -> SqlResult<Value> {
    eval(name, &[d, Value::Int(n), Value::Str(unit.into())]).expect("family owns the date math")
}

#[test]
fn date_add_units() {
    // Day unit on a DATE stays a DATE.
    assert_eq!(
        add("date_add", Value::Date(days(2024, 1, 2)), 3, "DAY"),
        Ok(Value::Date(days(2024, 1, 5)))
    );
    // Month addition clamps to the month end (Jan 31 + 1 MONTH).
    assert_eq!(
        add("date_add", Value::Date(days(2024, 1, 31)), 1, "MONTH"),
        Ok(Value::Date(days(2024, 2, 29)))
    );
    // Year addition clamps leap days.
    assert_eq!(
        add("date_add", Value::Date(days(2024, 2, 29)), 1, "YEAR"),
        Ok(Value::Date(days(2025, 2, 28)))
    );
    // A time unit promotes a DATE to midnight DATETIME.
    assert_eq!(
        add("date_add", Value::Date(days(2024, 1, 2)), 25, "HOUR"),
        Ok(dt(2024, 1, 3, 1, 0, 0))
    );
    // DATETIME inputs stay DATETIME across all units.
    assert_eq!(
        add("date_add", dt(2024, 1, 2, 3, 4, 5), 90, "MINUTE"),
        Ok(dt(2024, 1, 2, 4, 34, 5))
    );
    assert_eq!(
        add("date_add", dt(2024, 1, 2, 3, 4, 5), -10, "SECOND"),
        Ok(dt(2024, 1, 2, 3, 3, 55))
    );
    // Microsecond unit rides the native storage.
    assert_eq!(
        add(
            "date_add",
            dt(2024, 1, 2, 3, 4, 5),
            1_000_000,
            "MICROSECOND"
        ),
        Ok(dt(2024, 1, 2, 3, 4, 6))
    );
    // Subtraction mirrors.
    assert_eq!(
        add("date_sub", Value::Date(days(2024, 1, 2)), 1, "DAY"),
        Ok(Value::Date(days(2024, 1, 1)))
    );
}

#[test]
fn date_add_mirrors_str_and_int_inputs() {
    // Str in -> canonical Str out; a bare-date spelling whose result
    // is still midnight answers in the bare-date form.
    assert_eq!(
        add("date_add", Value::Str("2024-01-31".into()), 1, "MONTH"),
        Ok(Value::Str("2024-02-29".into()))
    );
    assert_eq!(
        add(
            "date_add",
            Value::Str("2024-01-31 00:00:00".into()),
            1,
            "DAY"
        ),
        Ok(Value::Str("2024-02-01 00:00:00".into()))
    );
    // Int in -> compact int out.
    assert_eq!(
        add("adddate", Value::Int(20240102), 1, "DAY"),
        Ok(Value::Int(20240103000000))
    );
    // The plain-days ADDDATE/SUBDATE spelling (2 args).
    assert_eq!(
        eval("adddate", &[Value::Date(days(2024, 1, 2)), Value::Int(3)]).unwrap(),
        Ok(Value::Date(days(2024, 1, 5)))
    );
    assert_eq!(
        eval("subdate", &[Value::Date(days(2024, 1, 2)), Value::Int(3)]).unwrap(),
        Ok(Value::Date(days(2023, 12, 30)))
    );
    // NULL / unparseable -> NULL; a bad unit is loud.
    assert_eq!(add("date_add", Value::Null, 1, "DAY"), Ok(Value::Null));
    assert_eq!(
        add("date_add", Value::Str("junk".into()), 1, "DAY"),
        Ok(Value::Null)
    );
    assert!(add("date_add", Value::Date(0), 1, "FORTNIGHT").is_err());
}

#[test]
fn datediff_sign_follows_order() {
    let f = |a: Value, b: Value| eval("datediff", &[a, b]).unwrap();
    assert_eq!(
        f(Value::Date(days(2024, 1, 2)), Value::Date(days(2024, 1, 1))),
        Ok(Value::Int(1))
    );
    assert_eq!(
        f(Value::Date(days(2024, 1, 1)), Value::Date(days(2024, 1, 2))),
        Ok(Value::Int(-1))
    );
    // Date parts only: the time-of-day never counts.
    assert_eq!(
        f(
            Value::Str("2024-01-02 23:00:00".into()),
            Value::Str("2024-01-01 00:00:00".into())
        ),
        Ok(Value::Int(1))
    );
    assert_eq!(f(Value::Null, Value::Date(0)), Ok(Value::Null));
    assert_eq!(
        f(Value::Str("junk".into()), Value::Date(0)),
        Ok(Value::Null)
    );
}

#[test]
fn date_format_common_specifiers() {
    let f = |d: Value, fmt: &str| eval("date_format", &[d, Value::Str(fmt.into())]).unwrap();
    let leap = dt(2024, 2, 29, 13, 5, 9);
    assert_eq!(
        f(leap.clone(), "%Y-%m-%d"),
        Ok(Value::Str("2024-02-29".into()))
    );
    assert_eq!(f(leap.clone(), "%y"), Ok(Value::Str("24".into())));
    assert_eq!(
        f(leap.clone(), "%H:%i:%s"),
        Ok(Value::Str("13:05:09".into()))
    );
    assert_eq!(f(leap.clone(), "%T"), Ok(Value::Str("13:05:09".into())));
    assert_eq!(f(leap.clone(), "%p"), Ok(Value::Str("PM".into())));
    // 1970-01-01 is a Thursday.
    assert_eq!(f(Value::Date(0), "%W"), Ok(Value::Str("Thursday".into())));
    assert_eq!(f(leap.clone(), "%M"), Ok(Value::Str("February".into())));
    assert_eq!(f(leap.clone(), "%j"), Ok(Value::Str("060".into())));
    // %% escapes; unknown specifiers pass through literally.
    assert_eq!(f(leap.clone(), "%% %q"), Ok(Value::Str("% %q".into())));
    // A bare date is midnight for the time specs.
    assert_eq!(
        f(Value::Date(0), "%H %i %s %p"),
        Ok(Value::Str("00 00 00 AM".into()))
    );
    assert_eq!(f(Value::Null, "%Y"), Ok(Value::Null));
    assert_eq!(f(Value::Str("junk".into()), "%Y"), Ok(Value::Null));
}

#[test]
fn unix_timestamp_pair() {
    // UNIX_TIMESTAMP(<datetime>) truncates to whole seconds.
    assert_eq!(
        eval("unix_timestamp", &[dt(2024, 1, 2, 3, 4, 5)]).unwrap(),
        Ok(Value::Int(
            days(2024, 1, 2) * 86_400 + 3 * 3600 + 4 * 60 + 5
        ))
    );
    assert_eq!(
        eval("unix_timestamp", &[Value::Str("2024-01-02".into())]).unwrap(),
        Ok(Value::Int(days(2024, 1, 2) * 86_400))
    );
    assert_eq!(
        eval("unix_timestamp", &[Value::Str("junk".into())]).unwrap(),
        Ok(Value::Null)
    );
    // No-arg form reads the same UTC clock as NOW().
    let Some(Ok(Value::Int(now_secs))) = eval("unix_timestamp", &[]) else {
        panic!("int");
    };
    let expect = crate::sql::temporal::now_micros() / 1_000_000;
    assert!((now_secs - expect).abs() <= 2, "{now_secs} vs {expect}");
    // FROM_UNIXTIME round-trips into canonical text.
    assert_eq!(
        eval("from_unixtime", &[Value::Int(0)]).unwrap(),
        Ok(Value::Str("1970-01-01 00:00:00".into()))
    );
    // Out-of-range seconds -> NULL (MySQL).
    assert_eq!(
        eval("from_unixtime", &[Value::Int(i64::MAX / 10)]).unwrap(),
        Ok(Value::Null)
    );
    assert_eq!(
        eval("from_unixtime", &[Value::Null]).unwrap(),
        Ok(Value::Null)
    );
}
