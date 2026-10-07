//! Control family: the constant/session functions (VERSION,
//! LAST_INSERT_ID) and the two structural casts the executor routes
//! here because they are not plain value-level calls:
//!
//! - CASE needs lazy evaluation (WHEN conditions run in order and the
//!   matching THEN short-circuits the rest),
//! - CAST evaluates its operand first, then reuses the write-path
//!   coercion helpers (fit_column / rescale_decimal) for the numeric
//!   targets instead of duplicating them.

use crate::sql::exec::expr::eval as eval_expr;
use crate::sql::exec::expr::{cmp_values, coerce, truthy, ColumnScope};
use crate::sql::exec::expr_decimal::rescale_decimal;
use crate::sql::parse::ast::{CastSpec, Expr};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{format_decimal, SqlType, Value};
use crate::sql::temporal;

use super::wrong_param_count;

/// Evaluate one control-family function; `None` = name not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "version" => Some(version(name, args)),
        // AUTO_INCREMENT session function; see exec/sequence.rs.
        "last_insert_id" => Some(crate::sql::exec::sequence::last_insert_id_value(args)),
        _ => None,
    }
}

fn version(name: &str, args: &[Value]) -> SqlResult<Value> {
    if !args.is_empty() {
        return Err(wrong_param_count(name));
    }
    Ok(Value::Str(env!("CARGO_PKG_VERSION").to_string()))
}

/// The lazy control entries: IF / IFNULL / NULLIF / COALESCE. The
/// expression executor calls this BEFORE evaluating arguments, so the
/// untaken branches never run. `None` = not this family's business.
pub fn eval_lazy<S: ColumnScope>(
    name: &str,
    args: &[Expr],
    scope: &S,
    row: &[Value],
) -> Option<SqlResult<Value>> {
    match name {
        "if" => Some(lazy_if(name, args, scope, row)),
        "ifnull" => Some(lazy_ifnull(name, args, scope, row)),
        "nullif" => Some(lazy_nullif(name, args, scope, row)),
        "coalesce" => Some(lazy_coalesce(name, args, scope, row)),
        _ => None,
    }
}

/// IF(cond, a, b): NULL condition is not TRUE, so the ELSE side wins.
/// (MySQL's IF is a 3-valued test over "is the condition nonzero".)
fn lazy_if<S: ColumnScope>(
    name: &str,
    args: &[Expr],
    scope: &S,
    row: &[Value],
) -> SqlResult<Value> {
    let [c, a, b] = args else {
        return Err(wrong_param_count(name));
    };
    let cond = eval_expr(c, scope, row)?;
    let branch = if truthy(&cond)? { a } else { b };
    eval_expr(branch, scope, row)
}

/// IFNULL(a, b): a when a is not NULL (b never evaluated then).
fn lazy_ifnull<S: ColumnScope>(
    name: &str,
    args: &[Expr],
    scope: &S,
    row: &[Value],
) -> SqlResult<Value> {
    let [a, b] = args else {
        return Err(wrong_param_count(name));
    };
    let v = eval_expr(a, scope, row)?;
    if matches!(v, Value::Null) {
        eval_expr(b, scope, row)
    } else {
        Ok(v)
    }
}

/// NULLIF(a, b): NULL when a = b (plain `=`, so a NULL operand pair
/// stays NULL); b is only evaluated when a is non-NULL.
fn lazy_nullif<S: ColumnScope>(
    name: &str,
    args: &[Expr],
    scope: &S,
    row: &[Value],
) -> SqlResult<Value> {
    let [a, b] = args else {
        return Err(wrong_param_count(name));
    };
    let va = eval_expr(a, scope, row)?;
    if matches!(va, Value::Null) {
        return Ok(Value::Null);
    }
    let vb = eval_expr(b, scope, row)?;
    if matches!(vb, Value::Null) {
        return Ok(va);
    }
    if cmp_values(&va, &vb)?.is_eq() {
        Ok(Value::Null)
    } else {
        Ok(va)
    }
}

/// COALESCE: the first non-NULL argument (NULL when every one is).
fn lazy_coalesce<S: ColumnScope>(
    name: &str,
    args: &[Expr],
    scope: &S,
    row: &[Value],
) -> SqlResult<Value> {
    if args.is_empty() {
        return Err(wrong_param_count(name));
    }
    for a in args {
        let v = eval_expr(a, scope, row)?;
        if !matches!(v, Value::Null) {
            return Ok(v);
        }
    }
    Ok(Value::Null)
}

/// CASE, both forms. Simple CASE (operand present) compares `=` per
/// WHEN value -- NULL never matches anything, including WHEN NULL;
/// searched CASE takes the first WHEN whose condition is TRUE (NULL
/// conditions are unknown, hence unmatched). The first matching THEN
/// is the only result expression evaluated.
pub fn eval_case<S: ColumnScope>(
    operand: Option<&Expr>,
    branches: &[(Expr, Expr)],
    else_expr: Option<&Expr>,
    scope: &S,
    row: &[Value],
) -> SqlResult<Value> {
    if let Some(op) = operand {
        let v = eval_expr(op, scope, row)?;
        for (when, then) in branches {
            let w = eval_expr(when, scope, row)?;
            // Three-valued: a NULL on either side of the equality is
            // UNKNOWN, not a match (only the ELSE can catch it).
            if !matches!(v, Value::Null) && !matches!(w, Value::Null) && cmp_values(&v, &w)?.is_eq()
            {
                return eval_expr(then, scope, row);
            }
        }
    } else {
        for (cond, then) in branches {
            if truthy(&eval_expr(cond, scope, row)?)? {
                return eval_expr(then, scope, row);
            }
        }
    }
    match else_expr {
        Some(e) => eval_expr(e, scope, row),
        None => Ok(Value::Null), // no branch matched, no ELSE
    }
}

/// CAST/CONVERT to the narrow target set of [`CastSpec`]. The DECIMAL
/// target is exactly the write-path column coercion (reuse, not a
/// twin); SIGNED/UNSIGNED and CHAR(n) are cast-specific semantics.
pub fn eval_cast(v: Value, to: &CastSpec) -> SqlResult<Value> {
    if matches!(v, Value::Null) {
        return Ok(v);
    }
    match to {
        CastSpec::Signed => cast_int(v, false),
        CastSpec::Unsigned => cast_int(v, true),
        CastSpec::Char(n) => cast_char(v, *n),
        CastSpec::Decimal { precision, scale } => coerce(
            v,
            SqlType::Decimal {
                precision: *precision,
                scale: *scale,
            },
        ),
    }
}

/// Out-of-range error of an integer cast (MySQL 1690 style).
fn int_out_of_range(target: &str, shown: &str) -> SqlError {
    SqlError::new(
        ErrorCode::WrongValue,
        format!("Out of range value for CAST AS {target}: '{shown}'"),
    )
}

/// Cast any value to Int. Doubles and decimals round half away from
/// zero (MySQL: CAST(1.5 AS SIGNED) = 2); strings parse strictly and
/// fall back to the exact decimal parser; temporals expose their
/// compact YYYYMMDD[HHMMSS] form. `unsigned` bounds the result by
/// [0, i64::MAX] (the Value domain has no u64 -- wider results are a
/// loud error rather than a wrong signed display).
fn cast_int(v: Value, unsigned: bool) -> SqlResult<Value> {
    let (m, shown) = match &v {
        Value::Bool(b) => (i128::from(i64::from(*b)), b.to_string()),
        Value::Int(i) => (i128::from(*i), i.to_string()),
        // Rust's f64 Display/round: half away from zero both ways.
        Value::Double(d) => (int_from_double(*d, &v)?, format!("{d}")),
        Value::Decimal(m, s) => (
            rescale_decimal(*m, *s, 0)
                .map_err(|_| int_out_of_range("SIGNED", &format_decimal(*m, *s)))?,
            format_decimal(*m, *s),
        ),
        Value::Str(s) => (int_from_str(s)?, s.clone()),
        Value::Bytes(b) => {
            let s = String::from_utf8(b.clone()).map_err(|_| {
                SqlError::new(ErrorCode::BadNull, "blob is not valid utf8".to_string())
            })?;
            (int_from_str(&s)?, s)
        }
        Value::Date(d) => (
            temporal::compact_date(*d)
                .map(i128::from)
                .ok_or_else(|| int_out_of_range("SIGNED", &temporal::format_date(*d)))?,
            temporal::format_date(*d),
        ),
        Value::DateTime(us) => (
            temporal::compact_datetime(*us)
                .map(i128::from)
                .ok_or_else(|| int_out_of_range("SIGNED", &temporal::format_datetime(*us)))?,
            temporal::format_datetime(*us),
        ),
        Value::Null => unreachable!("caller handled NULL"),
    };
    let target = if unsigned { "UNSIGNED" } else { "SIGNED" };
    if unsigned && m < 0 {
        // MySQL would wrap into u64 (18446744073709551615 - n + 1);
        // the i64-only value domain rejects loudly instead.
        return Err(int_out_of_range(target, &shown));
    }
    i64::try_from(m)
        .map(Value::Int)
        .map_err(|_| int_out_of_range(target, &shown))
}

fn int_from_double(d: f64, v: &Value) -> SqlResult<i128> {
    if !d.is_finite() || d >= 9.3e18 || d <= -9.3e18 {
        return Err(int_out_of_range("SIGNED", &format!("{v:?}")));
    }
    Ok(d.round() as i128)
}

/// Strict integer parse of a string, falling back to the exact
/// decimal parser with half-away rescaling ('12.5' -> 13).
fn int_from_str(s: &str) -> SqlResult<i128> {
    let t = s.trim_ascii();
    if let Ok(i) = t.parse::<i64>() {
        return Ok(i128::from(i));
    }
    match Value::parse_decimal(t) {
        Ok(Value::Decimal(m, scale)) => rescale_decimal(m, scale, 0).map_err(|_| {
            SqlError::new(
                ErrorCode::WrongValue,
                format!("Incorrect integer value: '{s}'"),
            )
        }),
        _ => Err(SqlError::new(
            ErrorCode::WrongValue,
            format!("Incorrect integer value: '{s}'"),
        )),
    }
}

/// CAST AS CHAR(n): render in the value's canonical text form (the
/// shared `value_text` coercion), then truncate to n characters
/// (MySQL counts characters, not bytes).
fn cast_char(v: Value, n: Option<u32>) -> SqlResult<Value> {
    let s = super::string::value_text(&v)?;
    Ok(Value::Str(match n {
        Some(n) => s.chars().take(n as usize).collect(),
        None => s,
    }))
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
