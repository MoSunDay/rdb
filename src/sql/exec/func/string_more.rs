//! Second half of the string family: SUBSTRING, LPAD/RPAD, LOCATE/
//! INSTR, REPLACE and the TRIM forms. Split from `string.rs` at the
//! file budget; the two halves share `value_text`/`text_arg`/`int_arg`
//! coercions and the NULL-propagates convention.

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;

use super::string::{int_arg, text_arg};
use super::wrong_param_count;

/// Evaluate one string_more function; `None` = name not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "substring" => Some(substring(name, args)),
        "lpad" => Some(pad(name, args, true)),
        "rpad" => Some(pad(name, args, false)),
        "locate" => Some(locate(name, args)),
        "instr" => Some(instr(name, args)),
        "replace" => Some(replace(name, args)),
        "trim" => Some(trim(name, args)),
        _ => None,
    }
}

/// SUBSTRING(s, pos[, len]): 1-based, negative pos counts from the
/// end, pos 0 / past-the-end / len <= 0 all yield ''.
fn substring(name: &str, args: &[Value]) -> SqlResult<Value> {
    let invalid = || wrong_param_count(name);
    let (s, pos, len) = match args {
        [s, pos] => (s, pos, None),
        [s, pos, len] => (s, pos, Some(len)),
        _ => return Err(invalid()),
    };
    if matches!(s, Value::Null) || matches!(pos, Value::Null) {
        return Ok(Value::Null);
    }
    if let Some(l) = len {
        if matches!(l, Value::Null) {
            return Ok(Value::Null);
        }
    }
    let chars: Vec<char> = text_arg(s)?.chars().collect();
    let total = chars.len() as i64;
    let pos = int_arg(pos)?;
    let len = match len {
        Some(l) => int_arg(l)?,
        None => total,
    };
    // 1-based start; negative wraps to total+1+pos.
    let start0 = if pos < 0 { total + 1 + pos } else { pos } - 1;
    if start0 < 0 || start0 >= total || len <= 0 {
        return Ok(Value::Str(String::new()));
    }
    let take = len.min(total - start0) as usize;
    Ok(Value::Str(
        chars[start0 as usize..start0 as usize + take]
            .iter()
            .collect(),
    ))
}

/// LPAD/RPAD(s, len, pad): len < s truncates; the pad repeats; NULL
/// pad (or an empty pad that would be needed) is NULL, like MySQL.
fn pad(name: &str, args: &[Value], left: bool) -> SqlResult<Value> {
    let [s, len, pad] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(s, Value::Null) || matches!(len, Value::Null) || matches!(pad, Value::Null) {
        return Ok(Value::Null);
    }
    let len = int_arg(len)?;
    if len < 0 {
        return Ok(Value::Null);
    }
    let len = len as usize;
    let s: Vec<char> = text_arg(s)?.chars().collect();
    let pad: Vec<char> = text_arg(pad)?.chars().collect();
    if s.len() >= len {
        // Truncate to exactly `len` characters (MySQL keeps the head).
        return Ok(Value::Str(s[..len].iter().collect()));
    }
    if pad.is_empty() {
        return Ok(Value::Null);
    }
    let mut filled: Vec<char> = Vec::with_capacity(len);
    let need = len - s.len();
    while filled.len() < need {
        let take = (need - filled.len()).min(pad.len());
        filled.extend_from_slice(&pad[..take]);
    }
    if left {
        filled.extend_from_slice(&s);
        Ok(Value::Str(filled.into_iter().collect()))
    } else {
        filled.splice(0..0, s.iter().copied());
        Ok(Value::Str(filled.into_iter().collect()))
    }
}

/// LOCATE(substr, s[, pos]): 1-based, 0 when absent, NULL operands
/// are NULL, a start position < 1 never matches. INSTR(s, substr) is
/// the same search with the arguments flipped.
fn locate(name: &str, args: &[Value]) -> SqlResult<Value> {
    let (needle, hay, from) = match args {
        [n, h] => (n, h, 1),
        [n, h, from] => (n, h, int_arg(from)?),
        _ => return Err(wrong_param_count(name)),
    };
    if matches!(needle, Value::Null) || matches!(hay, Value::Null) {
        return Ok(Value::Null);
    }
    let needle = text_arg(needle)?;
    let hay = text_arg(hay)?;
    Ok(Value::Int(find_from(&hay, &needle, from)))
}

fn instr(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [hay, needle] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(needle, Value::Null) || matches!(hay, Value::Null) {
        return Ok(Value::Null);
    }
    let needle = text_arg(needle)?;
    let hay = text_arg(hay)?;
    Ok(Value::Int(find_from(&hay, &needle, 1)))
}

/// Character-index (1-based) of `needle` in `hay` at/after `from`.
fn find_from(hay: &str, needle: &str, from: i64) -> i64 {
    if from < 1 {
        return 0;
    }
    let hay_chars: Vec<char> = hay.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let start = (from - 1) as usize;
    if start > hay_chars.len() {
        return 0;
    }
    if needle_chars.is_empty() {
        // MySQL: the empty needle matches at the start position.
        return start as i64 + 1;
    }
    let mut i = start;
    while i + needle_chars.len() <= hay_chars.len() {
        if hay_chars[i..i + needle_chars.len()] == needle_chars[..] {
            return i as i64 + 1;
        }
        i += 1;
    }
    0
}

/// REPLACE(s, from, to): empty `from` leaves s unchanged; any NULL
/// operand is NULL.
fn replace(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [s, from, to] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(s, Value::Null) || matches!(from, Value::Null) || matches!(to, Value::Null) {
        return Ok(Value::Null);
    }
    let s = text_arg(s)?;
    let from = text_arg(from)?;
    let to = text_arg(to)?;
    if from.is_empty() {
        return Ok(Value::Str(s));
    }
    Ok(Value::Str(s.replace(&from, &to)))
}

/// TRIM(s) strips spaces; TRIM(s, remstr, BOTH|LEADING|TRAILING)
/// (translated from the `TRIM(.. FROM ..)` special form) strips
/// repeated remstr occurrences on that side.
fn trim(name: &str, args: &[Value]) -> SqlResult<Value> {
    match args {
        [s] => {
            if matches!(s, Value::Null) {
                return Ok(Value::Null);
            }
            Ok(Value::Str(text_arg(s)?.trim_matches(' ').to_string()))
        }
        [s, remstr, mode] => {
            if matches!(s, Value::Null) || matches!(remstr, Value::Null) {
                return Ok(Value::Null);
            }
            let s = text_arg(s)?;
            let rem = text_arg(remstr)?;
            let Value::Str(m) = mode else {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("trim mode {mode:?}"),
                ));
            };
            // MySQL strips repeated occurrences of the whole remstr
            // unit (a char set only when remstr is one char long).
            let out = match m.as_str() {
                "BOTH" => cut_suffix(cut_prefix(&s, &rem), &rem),
                "LEADING" => cut_prefix(&s, &rem),
                "TRAILING" => cut_suffix(&s, &rem),
                other => {
                    return Err(SqlError::new(
                        ErrorCode::NotSupported,
                        format!("trim mode {other}"),
                    ))
                }
            };
            Ok(Value::Str(out.to_string()))
        }
        _ => Err(wrong_param_count(name)),
    }
}

/// Repeatedly drop a whole-string prefix/suffix (the remstr unit).
fn cut_prefix<'a>(t: &'a str, rem: &str) -> &'a str {
    let mut t = t;
    if !rem.is_empty() {
        while t.starts_with(rem) {
            t = &t[rem.len()..];
        }
    }
    t
}

fn cut_suffix<'a>(t: &'a str, rem: &str) -> &'a str {
    let mut t = t;
    if !rem.is_empty() {
        while t.ends_with(rem) {
            t = &t[..t.len() - rem.len()];
        }
    }
    t
}

#[cfg(test)]
#[path = "string_more_tests.rs"]
mod tests;
