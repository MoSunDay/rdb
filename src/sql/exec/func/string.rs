//! String-function family: length/char_length/upper/lower, the
//! byte-oriented REGEXP/RLIKE matcher used by `Expr::Regexp`, and the
//! first half of the MySQL editing set (CONCAT family, LEFT/RIGHT,
//! REPEAT/REVERSE, HEX/UNHEX). The remainder (SUBSTRING/LPAD/RPAD/
//! LOCATE/REPLACE/TRIM) lives in `string_more` to stay under the file
//! budget; both halves share the `value_text` coercion below.

use regex::bytes::Regex;

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{format_decimal, Value};
use crate::sql::temporal;

use super::wrong_param_count;

/// Evaluate one string-family function; `None` = name not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "length" | "char_length" => Some(length(name, args)),
        "upper" => Some(change_case(name, args, true)),
        "lower" => Some(change_case(name, args, false)),
        "concat" => Some(concat(name, args)),
        "concat_ws" => Some(concat_ws(name, args)),
        "left" | "right" => Some(left_right(name, args, name == "left")),
        "repeat" => Some(repeat(name, args)),
        "reverse" => Some(reverse(name, args)),
        "hex" => Some(hex(name, args)),
        "unhex" => Some(unhex(name, args)),
        _ => super::string_more::eval(name, args),
    }
}

/// Canonical text form of any non-NULL value (the coercion CONCAT,
/// GROUP_CONCAT and CAST AS CHAR share). Blobs must be valid utf8;
/// booleans render as MySQL's 0/1.
pub fn value_text(v: &Value) -> SqlResult<String> {
    Ok(match v {
        Value::Str(s) => s.clone(),
        Value::Bytes(b) => String::from_utf8(b.clone())
            .map_err(|_| SqlError::new(ErrorCode::BadNull, "blob is not valid utf8".to_string()))?,
        Value::Bool(b) => i64::from(*b).to_string(),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => format!("{d}"),
        Value::Decimal(m, s) => format_decimal(*m, *s),
        Value::Date(d) => temporal::format_date(*d),
        Value::DateTime(us) => temporal::format_datetime(*us),
        Value::Null => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                "NULL has no text form".to_string(),
            ))
        }
    })
}

/// CONCAT: NULL anywhere is NULL; everything else coerces to text.
fn concat(name: &str, args: &[Value]) -> SqlResult<Value> {
    if args.is_empty() {
        return Err(wrong_param_count(name));
    }
    let mut out = String::new();
    for v in args {
        if matches!(v, Value::Null) {
            return Ok(Value::Null);
        }
        out.push_str(&value_text(v)?);
    }
    Ok(Value::Str(out))
}

/// CONCAT_WS: NULL separator is NULL; NULL args are skipped (the one
/// NULL-resistant string function).
fn concat_ws(name: &str, args: &[Value]) -> SqlResult<Value> {
    if args.len() < 2 {
        return Err(wrong_param_count(name));
    }
    if matches!(args[0], Value::Null) {
        return Ok(Value::Null);
    }
    let sep = value_text(&args[0])?;
    let mut out = String::new();
    // A separator sits between emitted args, not between non-empty
    // ones: an empty string is a real argument, so `emitted` (not
    // !out.is_empty()) decides whether one is due.
    let mut emitted = false;
    for v in &args[1..] {
        if matches!(v, Value::Null) {
            continue;
        }
        if emitted {
            out.push_str(&sep);
        }
        out.push_str(&value_text(v)?);
        emitted = true;
    }
    Ok(Value::Str(out))
}

/// LEFT/RIGHT count characters (MySQL), NULL-propagating; a count <= 0
/// is the empty string.
fn left_right(name: &str, args: &[Value], left: bool) -> SqlResult<Value> {
    let [s, n] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(s, Value::Null) || matches!(n, Value::Null) {
        return Ok(Value::Null);
    }
    let s = text_arg(s)?;
    let n = int_arg(n)?;
    let taken: String = if left {
        s.chars().take(n.max(0) as usize).collect()
    } else {
        let skip = s.chars().count().saturating_sub(n.max(0) as usize);
        s.chars().skip(skip).collect()
    };
    Ok(Value::Str(taken))
}

fn repeat(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [s, n] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(s, Value::Null) || matches!(n, Value::Null) {
        return Ok(Value::Null);
    }
    let s = text_arg(s)?;
    let n = int_arg(n)?.max(0);
    Ok(Value::Str(s.repeat(n as usize)))
}

fn reverse(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [s] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(s, Value::Null) {
        return Ok(Value::Null);
    }
    let s = text_arg(s)?;
    Ok(Value::Str(s.chars().rev().collect()))
}

/// HEX: strings/blobs hex-encode their bytes (uppercase, like MySQL);
/// integers hex-encode the 64-bit two's-complement word.
fn hex(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => Ok(Value::Str(format!("{:016X}", *i as u64))),
        Value::Str(s) => Ok(Value::Str(hex_upper(s.as_bytes()))),
        Value::Bytes(b) => Ok(Value::Str(hex_upper(b))),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("hex({other:?})"),
        )),
    }
}

fn hex_upper(b: &[u8]) -> String {
    // hex::encode is lowercase; MySQL prints the letters uppercase.
    hex::encode(b).to_uppercase()
}

/// UNHEX: hex text -> bytes; odd length or a non-hex digit is NULL
/// (MySQL returns NULL, not an error).
fn unhex(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    let Value::Str(s) = v else {
        return match v {
            Value::Null => Ok(Value::Null),
            other => Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("unhex({other:?})"),
            )),
        };
    };
    if s.len() % 2 != 0 {
        return Ok(Value::Null);
    }
    match hex::decode(s.to_lowercase()) {
        Ok(b) => Ok(Value::Bytes(b)),
        Err(_) => Ok(Value::Null),
    }
}

// ---- shared string-argument coercions (string_more reuses these) ----

/// Text form of a non-NULL argument (numbers/decimals/temporals ride
/// their canonical spelling, like CONCAT's coercion).
pub(super) fn text_arg(v: &Value) -> SqlResult<String> {
    value_text(v)
}

/// Integer argument of a string function (the count/position slot):
/// Ints pass through, Decimals round half-away at scale 0, Doubles
/// round; anything else is loud.
pub(super) fn int_arg(v: &Value) -> SqlResult<i64> {
    Ok(match v {
        Value::Int(i) => *i,
        // Double/Decimal counts: MySQL rounds to nearest.
        Value::Double(d) => {
            if d.is_finite() && d.abs() < 9.3e18 {
                d.round() as i64
            } else {
                return Err(SqlError::new(
                    ErrorCode::WrongValue,
                    format!("count out of range: {d}"),
                ));
            }
        }
        Value::Decimal(m, s) => i64::try_from(
            crate::sql::exec::expr_decimal::rescale_decimal(*m, *s, 0).map_err(|_| {
                SqlError::new(
                    ErrorCode::WrongValue,
                    format!("count out of range: {}", format_decimal(*m, *s)),
                )
            })?,
        )
        .map_err(|_| {
            SqlError::new(
                ErrorCode::WrongValue,
                format!("count out of range: {}", format_decimal(*m, *s)),
            )
        })?,
        other => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("integer argument {other:?}"),
            ))
        }
    })
}

/// MySQL: LENGTH() counts bytes, CHAR_LENGTH() counts characters.
fn length(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    match v {
        Value::Str(s) => Ok(Value::Int(if name == "char_length" {
            s.chars().count() as i64
        } else {
            s.len() as i64
        })),
        Value::Bytes(b) => Ok(Value::Int(b.len() as i64)),
        Value::Null => Ok(Value::Null),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("length({other:?})"),
        )),
    }
}

fn change_case(name: &str, args: &[Value], up: bool) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    match v {
        Value::Str(s) => Ok(Value::Str(if up {
            s.to_uppercase()
        } else {
            s.to_lowercase()
        })),
        Value::Null => Ok(Value::Null),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("{}({other:?})", if up { "upper" } else { "lower" }),
        )),
    }
}

/// `expr REGEXP pattern`: byte-oriented, case-sensitive (no collation
/// -- LIKE's own bytewise decision applies here too), an unanchored
/// search like MySQL. Either side NULL -> NULL; a bad pattern is a
/// loud wrong-value error. Result is Int 0/1 (MySQL wire shape).
pub fn regexp_match(value: &Value, pattern: &Value) -> SqlResult<Value> {
    if matches!(value, Value::Null) || matches!(pattern, Value::Null) {
        return Ok(Value::Null);
    }
    let (bytes, pat): (&[u8], &str) = match (value, pattern) {
        (Value::Str(s), Value::Str(p)) => (s.as_bytes(), p.as_str()),
        (Value::Bytes(b), Value::Str(p)) => (b.as_slice(), p.as_str()),
        _ => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                "REGEXP requires string operands".to_string(),
            ))
        }
    };
    let re = Regex::new(pat).map_err(|e| {
        SqlError::new(
            ErrorCode::WrongValue,
            format!("Incorrect REGEXP value: '{pat}' ({e})"),
        )
    })?;
    Ok(Value::Int(i64::from(re.is_match(bytes))))
}

#[cfg(test)]
#[path = "string_tests.rs"]
mod tests;
