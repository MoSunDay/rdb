//! Interval math + formatting: DATE_ADD/DATE_SUB/ADDDATE/SUBDATE with
//! INTERVAL, DATEDIFF, DATE_FORMAT and the UNIX_TIMESTAMP pair. All
//! date math reuses `sql::temporal`'s civil algorithms; results mirror
//! the input's storage shape (Date/DateTime/Str/Int-compact).

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;
use crate::sql::temporal::{self, MICROS_PER_DAY};

use super::datetime::temporal_value;
use super::string::int_arg;
use super::wrong_param_count;

/// Evaluate one datetime_more function; `None` = not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "date_add" | "adddate" => Some(date_add_sub(name, args, 1)),
        "date_sub" | "subdate" => Some(date_add_sub(name, args, -1)),
        "datediff" => Some(datediff(name, args)),
        "date_format" => Some(date_format(name, args)),
        "unix_timestamp" => Some(unix_timestamp(name, args)),
        "from_unixtime" => Some(from_unixtime(name, args)),
        _ => None,
    }
}

/// DATE_ADD/DATE_SUB/ADDDATE/SUBDATE. Translated shapes: the INTERVAL
/// form always arrives as 3 args `(dt, n, unit)`; the plain ADDDATE/
/// SUBDATE days form as `(dt, days)` (sign carries the direction).
fn date_add_sub(name: &str, args: &[Value], dir: i64) -> SqlResult<Value> {
    let (dt, n, unit) = match args {
        // ADDDATE(d, n): plain days, == INTERVAL n DAY.
        [dt, days] => (dt, days, &Value::Str("DAY".to_string())),
        [dt, n, unit] => (dt, n, unit),
        _ => return Err(wrong_param_count(name)),
    };
    if matches!(dt, Value::Null) || matches!(n, Value::Null) || matches!(unit, Value::Null) {
        return Ok(Value::Null);
    }
    let Some((days, us, as_int)) = temporal_value(dt) else {
        return Ok(Value::Null);
    };
    let n = int_arg(n)?.checked_mul(dir).ok_or_else(overflow)?;
    let Value::Str(unit) = unit else {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("{name} interval unit {unit:?}"),
        ));
    };
    let (days, us) = add_interval(unit, n, days, us)?;
    // Result mirrors the input shape: DATE + a pure-date unit stays a
    // DATE, a time unit promotes it to midnight DATETIME (MySQL).
    let time_unit = matches!(unit.as_str(), "HOUR" | "MINUTE" | "SECOND" | "MICROSECOND");
    Ok(if as_int {
        Value::Int(temporal::compact_datetime(us).ok_or_else(overflow)?)
    } else if matches!(dt, Value::Date(_)) {
        if time_unit {
            Value::DateTime(us)
        } else {
            Value::Date(days)
        }
    } else if matches!(dt, Value::DateTime(_)) {
        Value::DateTime(us)
    } else {
        // Text in, text out: a bare-date spelling whose result is
        // still midnight answers in the same bare-date form (MySQL
        // keeps the argument's rendering shape).
        let bare = matches!(dt, Value::Str(s) if temporal::parse_date(s).is_some())
            && us.rem_euclid(MICROS_PER_DAY) == 0;
        if bare {
            Value::Str(temporal::format_date(us.div_euclid(MICROS_PER_DAY)))
        } else {
            Value::Str(temporal::format_datetime(us))
        }
    })
}

fn overflow() -> SqlError {
    SqlError::new(
        ErrorCode::WrongValue,
        "Datetime function: datetime field overflow".to_string(),
    )
}

/// The interval units: YEAR/MONTH work on the civil (y, m, d) triple
/// with day clamping (MySQL: Jan 31 + 1 MONTH = Feb 29); the rest are
/// day/microsecond arithmetic. A Date input only stays a Date under
/// the pure-date units; time units promote it to midnight DATETIME.
fn add_interval(unit: &str, n: i64, days: i64, us: i64) -> SqlResult<(i64, i64)> {
    let time_of_day = us.rem_euclid(MICROS_PER_DAY);
    let day_us = |d: i64, t: i64| -> SqlResult<(i64, i64)> {
        let us = d
            .checked_mul(MICROS_PER_DAY)
            .and_then(|base| base.checked_add(t))
            .ok_or_else(overflow)?;
        Ok((us.div_euclid(MICROS_PER_DAY), us))
    };
    match unit {
        "YEAR" | "MONTH" => {
            let Some((y, m, d)) = temporal::civil_from_days(days) else {
                return Err(overflow());
            };
            let total = y * 12 + i64::from(m) - 1 + if unit == "YEAR" { n * 12 } else { n };
            let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            // Clamp to the target month's last day (MySQL semantics).
            let last = temporal::days_in_month(ny, nm as u32);
            let nd = (d as i64).min(i64::from(last)) as u32;
            let ndays = temporal::days_from_civil(ny, nm as u32, nd).ok_or_else(overflow)?;
            day_us(ndays, time_of_day)
        }
        "DAY" => day_us(days.checked_add(n).ok_or_else(overflow)?, time_of_day),
        "HOUR" | "MINUTE" | "SECOND" | "MICROSECOND" => {
            let micros = match unit {
                "HOUR" => n.checked_mul(3_600_000_000),
                "MINUTE" => n.checked_mul(60_000_000),
                "SECOND" => n.checked_mul(1_000_000),
                _ => Some(n),
            }
            .ok_or_else(overflow)?;
            let us = us.checked_add(micros).ok_or_else(overflow)?;
            Ok((us.div_euclid(MICROS_PER_DAY), us))
        }
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("INTERVAL unit {other}"),
        )),
    }
}

/// DATEDIFF(a, b): date parts only, a - b in days (a NULL or
/// unparseable side is NULL).
fn datediff(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [a, b] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let (Some((da, _, _)), Some((db, _, _))) = (temporal_value(a), temporal_value(b)) else {
        return Ok(Value::Null);
    };
    Ok(Value::Int(da.checked_sub(db).ok_or_else(overflow)?))
}

/// DATE_FORMAT(d, fmt): the common specifiers; `%%` escapes, an
/// unknown specifier passes through literally. NULL operands are
/// NULL; an unparseable date is NULL (permissive read path).
fn date_format(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [d, fmt] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(d, Value::Null) || matches!(fmt, Value::Null) {
        return Ok(Value::Null);
    }
    let Some((days, us, _)) = temporal_value(d) else {
        return Ok(Value::Null);
    };
    let Some((y, m, day)) = temporal::civil_from_days(days) else {
        return Ok(Value::Null);
    };
    let secs = us.rem_euclid(MICROS_PER_DAY) / 1_000_000;
    let fmt = match fmt {
        Value::Str(s) => s.as_str(),
        other => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("date_format format {other:?}"),
            ))
        }
    };
    Ok(Value::Str(format_specifiers(fmt, days, y, m, day, secs)))
}

fn format_specifiers(fmt: &str, days: i64, y: i64, m: u32, d: u32, secs: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    // Sunday-first indexing: (epoch_days + 4) % 7 lands 1970-01-01 (a
    // Thursday) on index 4.
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let mut out = String::with_capacity(fmt.len());
    let mut chars = fmt.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('Y') => out.push_str(&format!("{y:04}")),
            Some('y') => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('H') => out.push_str(&format!("{:02}", secs / 3600)),
            Some('i') => out.push_str(&format!("{:02}", (secs / 60) % 60)),
            Some('s') => out.push_str(&format!("{:02}", secs % 60)),
            Some('T') => out.push_str(&format!(
                "{:02}:{:02}:{:02}",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60
            )),
            Some('p') => out.push_str(if secs / 3600 < 12 { "AM" } else { "PM" }),
            Some('W') => out.push_str(DAYS[(days + 4).rem_euclid(7) as usize]),
            Some('M') => out.push_str(MONTHS[(m - 1) as usize]),
            Some('j') => {
                let jan1 = temporal::days_from_civil(y, 1, 1).unwrap_or(days);
                out.push_str(&format!("{:03}", days - jan1 + 1));
            }
            // Unknown specifier: emit the '%' and the char literally.
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

/// UNIX_TIMESTAMP(): now, in whole seconds (UTC convention of the
/// clock family); UNIX_TIMESTAMP(str/int/date) parses then truncates
/// to seconds. Unparseable -> NULL.
fn unix_timestamp(name: &str, args: &[Value]) -> SqlResult<Value> {
    match args {
        [] => Ok(Value::Int(temporal::now_micros() / 1_000_000)),
        [v] => match temporal_value(v) {
            Some((_, us, _)) => Ok(Value::Int(us.div_euclid(1_000_000))),
            None => Ok(Value::Null),
        },
        _ => Err(wrong_param_count(name)),
    }
}

/// FROM_UNIXTIME(u): whole seconds since the epoch -> the canonical
/// DATETIME text (out-of-range seconds are NULL, like MySQL).
fn from_unixtime(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    let secs = int_arg(v)?;
    let Some(us) = secs.checked_mul(1_000_000) else {
        return Ok(Value::Null);
    };
    match temporal::civil_from_days(us.div_euclid(MICROS_PER_DAY)) {
        Some(_) => Ok(Value::Str(temporal::format_datetime(us))),
        None => Ok(Value::Null),
    }
}

#[cfg(test)]
#[path = "datetime_more_tests.rs"]
mod tests;
