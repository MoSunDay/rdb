//! Clock + extraction family: the UTC wall-clock entries (NOW family,
//! CURDATE family, CURTIME family) and the DATE/YEAR/MONTH/DAY/HOUR/
//! MINUTE/SECOND extractors over the civil math in `sql::temporal`.
//! The DATE_ADD/DATEDIFF/DATE_FORMAT/UNIX_TIMESTAMP set lives in
//! `datetime_more`. No session timezone exists, so all spellings of
//! "current time" read the same UTC clock; the optional fsp argument
//! parses but is ignored (microsecond storage either way).
//!
//! Temporal inputs parse permissively: a Str/Int that does not spell a
//! date/datetime yields NULL (MySQL returns NULL with a warning for
//! these read-path functions, unlike the loud write-path coercion).

use crate::sql::parse::error::SqlResult;
use crate::sql::storage::schema::Value;
use crate::sql::temporal;

use super::wrong_param_count;

/// Evaluate one clock/extraction function; `None` = not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "now" | "current_timestamp" | "sysdate" | "localtime" | "localtimestamp" => {
            Some(now(name, args))
        }
        "curdate" | "current_date" => Some(curdate(name, args)),
        "curtime" | "current_time" => Some(curtime(name, args)),
        "date" => Some(date(name, args)),
        "year" | "month" | "day" | "dayofmonth" => Some(date_part(name, args)),
        "hour" | "minute" | "second" => Some(time_part(name, args)),
        _ => super::datetime_more::eval(name, args),
    }
}

fn now(name: &str, args: &[Value]) -> SqlResult<Value> {
    if args.len() > 1 {
        return Err(wrong_param_count(name));
    }
    Ok(Value::DateTime(temporal::now_micros()))
}

fn curdate(name: &str, args: &[Value]) -> SqlResult<Value> {
    if args.len() > 1 {
        return Err(wrong_param_count(name));
    }
    Ok(Value::Date(temporal::today_days()))
}

/// CURTIME: no TIME storage type exists, so the time-of-day renders as
/// its canonical `HH:MM:SS` text (MySQL's wire shape for TIME anyway).
fn curtime(name: &str, args: &[Value]) -> SqlResult<Value> {
    if args.len() > 1 {
        return Err(wrong_param_count(name));
    }
    let secs = temporal::now_micros().rem_euclid(temporal::MICROS_PER_DAY) / 1_000_000;
    Ok(Value::Str(hms(secs)))
}

/// `HH:MM:SS` of a seconds-of-day count.
pub(super) fn hms(secs: i64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// DATE(expr): the date part of any temporal spelling; invalid -> NULL.
fn date(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    Ok(match temporal_value(v) {
        Some((days, _us, _as_int)) => Value::Date(days),
        // NULL stays NULL; an unparseable spelling is NULL too
        // (MySQL's permissive read path, see the module docs).
        None => Value::Null,
    })
}

/// YEAR/MONTH/DAY(DAYOFMONTH): civil fields of the date part.
fn date_part(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    let Some((days, _, _)) = temporal_value(v) else {
        return Ok(Value::Null);
    };
    let Some((y, m, d)) = temporal::civil_from_days(days) else {
        return Ok(Value::Null);
    };
    Ok(Value::Int(match name {
        "year" => y,
        "month" => i64::from(m),
        _ => i64::from(d),
    }))
}

/// HOUR/MINUTE/SECOND: fields of the time part (a bare date is
/// midnight, so 0/0/0).
fn time_part(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    let Some((_, us, _)) = temporal_value(v) else {
        return Ok(Value::Null);
    };
    let secs = us.rem_euclid(temporal::MICROS_PER_DAY) / 1_000_000;
    Ok(Value::Int(match name {
        "hour" => secs / 3600,
        "minute" => (secs / 60) % 60,
        _ => secs % 60,
    }))
}

/// One temporal input in both storage shapes: `(days, micros,
/// compact-int)`. The int flag lets DATE_ADD mirror an Int input; NULL
/// input is NULL; anything unparseable is None (callers choose NULL).
pub(super) fn temporal_value(v: &Value) -> Option<(i64, i64, bool)> {
    let (us, as_int) = match v {
        Value::Null => return None,
        Value::Date(d) => (d.checked_mul(temporal::MICROS_PER_DAY)?, false),
        Value::DateTime(us) => (*us, false),
        Value::Str(s) => (temporal::parse_datetime(s)?, false),
        // A bare YYYYMMDD int is midnight of that day (MySQL accepts
        // both the 8- and 14-digit compact spellings).
        Value::Int(i) => (
            temporal::parse_compact_datetime(*i).or_else(|| {
                temporal::parse_compact_date(*i)
                    .and_then(|d| d.checked_mul(temporal::MICROS_PER_DAY))
            })?,
            true,
        ),
        _ => return None,
    };
    Some((us.div_euclid(temporal::MICROS_PER_DAY), us, as_int))
}

#[cfg(test)]
#[path = "datetime_tests.rs"]
mod tests;
