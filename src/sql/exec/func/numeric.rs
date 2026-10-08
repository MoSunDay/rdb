//! Numeric-function family: abs, the rounding set (ROUND/CEIL/FLOOR/
//! TRUNCATE, exact on Decimal inputs) and the value-level bit
//! operators (`& | ^ << >>`) shared by `eval_binop`. MOD/POW/SQRT/
//! SIGN/GREATEST/LEAST live in `numeric_more`.

use crate::sql::parse::ast::BinOp;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{format_decimal, Value};

use super::string::int_arg;
use super::wrong_param_count;
use crate::sql::exec::expr::bigint_out_of_range;
use crate::sql::exec::expr_decimal::{decimal_out_of_range, pow10, rescale_decimal};

/// Evaluate one numeric-family function; `None` = name not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "abs" => Some(abs(name, args)),
        "round" => Some(round(name, args)),
        "ceil" | "ceiling" => Some(ceil_floor(name, args, true)),
        "floor" => Some(ceil_floor(name, args, false)),
        "truncate" => Some(truncate(name, args)),
        _ => super::numeric_more::eval(name, args),
    }
}

fn abs(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    match v {
        // i64::MIN has no positive peer: loud 1690-style, never wrapped.
        Value::Int(i) => {
            Ok(Value::Int(i.checked_abs().ok_or_else(|| {
                bigint_out_of_range(&format!("ABS({i})"))
            })?))
        }
        Value::Double(d) => Ok(Value::Double(d.abs())),
        // i128::MIN has no positive peer: loud, never wrapped.
        Value::Decimal(m, s) => Ok(Value::Decimal(
            m.checked_abs()
                .ok_or_else(|| decimal_out_of_range(&format_decimal(*m, *s), *s))?,
            *s,
        )),
        Value::Null => Ok(Value::Null),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("abs({other:?})"),
        )),
    }
}

/// ROUND(x[, d]): half away from zero on exact inputs, f64 rounding
/// on doubles, Int stays Int. A negative d scales the integer part
/// down (result scale 0). ROUND(2.5) = 3, ROUND(-2.5) = -3.
fn round(name: &str, args: &[Value]) -> SqlResult<Value> {
    let (x, d) = match args {
        [x] => (x, 0),
        // NULL digits is NULL before the count coercion (MySQL), so
        // the loud int_arg error never fires on a NULL scale.
        [_, Value::Null] => return Ok(Value::Null),
        [x, d] => (x, int_arg(d)?),
        _ => return Err(wrong_param_count(name)),
    };
    let d = i32::try_from(d).unwrap_or(i32::MAX);
    match x {
        Value::Null => Ok(Value::Null),
        Value::Int(i) => {
            if d >= 0 {
                return Ok(Value::Int(*i));
            }
            let f = i128::checked_pow(10, u32::try_from(-(d as i64)).unwrap_or(38)).unwrap_or(0);
            let q = round_half_away(i128::from(*i), f);
            Ok(Value::Int(
                i64::try_from(q * f).map_err(|_| out_of_range(x))?,
            ))
        }
        Value::Decimal(m, s) => {
            if d >= 0 {
                let to = u8::try_from(d).unwrap_or(*s).min(*s);
                return Ok(Value::Decimal(rescale_decimal(*m, *s, to)?, to));
            }
            // Scale the integer part down: drop s+|d| digits (rounded),
            // re-append |d| zeros, land at scale 0.
            let drop = i32::from(*s) - d;
            let f = i128::checked_pow(10, u32::try_from(drop as i64).unwrap_or(38)).unwrap_or(0);
            let zeros =
                i128::checked_pow(10, u32::try_from(-(d as i64)).unwrap_or(38)).unwrap_or(0);
            let q = round_half_away(*m, f);
            Ok(Value::Decimal(
                q.checked_mul(zeros).ok_or_else(|| out_of_range(x))?,
                0,
            ))
        }
        Value::Double(dv) => Ok(Value::Double(if d >= 0 {
            let f = 10f64.powi(d);
            (dv * f).round() / f
        } else {
            let f = 10f64.powi(-d);
            (dv / f).round() * f
        })),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("round({other:?})"),
        )),
    }
}

/// Quotient of m/f rounded half away from zero (f > 0 or 0 = "way
/// coarser than the mantissa", where the quotient is 0).
fn round_half_away(m: i128, f: i128) -> i128 {
    if f == 0 {
        return 0;
    }
    let q = m / f;
    let r = m % f;
    if r != 0 && r.unsigned_abs() * 2 >= f.unsigned_abs() {
        if m < 0 {
            q - 1
        } else {
            q + 1
        }
    } else {
        q
    }
}

fn out_of_range(v: &Value) -> SqlError {
    SqlError::new(
        ErrorCode::WrongValue,
        format!("Out of range value in rounding: {v:?}"),
    )
}

/// CEIL/CEILING and FLOOR: exact on Decimal (scale-0 result, the
/// MySQL NEWDECIMAL(.,0) shape), f64 on doubles, identity on Int.
fn ceil_floor(name: &str, args: &[Value], up: bool) -> SqlResult<Value> {
    let [x] = args else {
        return Err(wrong_param_count(name));
    };
    match x {
        Value::Null => Ok(Value::Null),
        Value::Int(_) => Ok(x.clone()),
        Value::Decimal(m, s) => {
            let p = pow10(*s);
            let (q, r) = (m / p, m % p);
            // ceil keeps q for exact and negative values, bumps
            // positives with a remainder; floor mirrors.
            let v = match (r == 0, up) {
                (true, _) => q,
                (_, true) if *m > 0 => q + 1,
                (_, false) if *m < 0 => q - 1,
                _ => q,
            };
            Ok(Value::Decimal(v, 0))
        }
        Value::Double(d) => Ok(Value::Double(if up { d.ceil() } else { d.floor() })),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("{name}({other:?})"),
        )),
    }
}

/// TRUNCATE(x, d): toward zero, exact on Decimal/Int; a negative d
/// truncates the integer part (result scale 0).
fn truncate(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [x, dv] = args else {
        return Err(wrong_param_count(name));
    };
    // NULL in either slot is NULL (MySQL) before any coercion.
    if matches!(x, Value::Null) || matches!(dv, Value::Null) {
        return Ok(Value::Null);
    }
    let d = int_arg(dv)?;
    let d = i32::try_from(d).unwrap_or(i32::MAX);
    match x {
        Value::Int(i) => {
            if d >= 0 {
                return Ok(Value::Int(*i));
            }
            let f = i128::checked_pow(10, u32::try_from(-(d as i64)).unwrap_or(38)).unwrap_or(0);
            let q = if f == 0 { 0 } else { i128::from(*i) / f };
            Ok(Value::Int(
                i64::try_from(q * f).map_err(|_| out_of_range(x))?,
            ))
        }
        Value::Decimal(m, s) => {
            if d >= 0 {
                let to = u8::try_from(d).unwrap_or(*s).min(*s);
                let drop = *s - to;
                let f = pow10(drop);
                return Ok(Value::Decimal(m / f, to));
            }
            let drop = i32::from(*s) - d;
            let f = i128::checked_pow(10, u32::try_from(drop as i64).unwrap_or(38)).unwrap_or(0);
            let zeros =
                i128::checked_pow(10, u32::try_from(-(d as i64)).unwrap_or(38)).unwrap_or(0);
            let q = if f == 0 { 0 } else { m / f };
            Ok(Value::Decimal(
                q.checked_mul(zeros).ok_or_else(|| out_of_range(x))?,
                0,
            ))
        }
        Value::Double(dv2) => Ok(Value::Double(if d >= 0 {
            let f = 10f64.powi(d);
            (dv2 * f).trunc() / f
        } else {
            let f = 10f64.powi(-d);
            (dv2 / f).trunc() * f
        })),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("truncate({other:?})"),
        )),
    }
}

/// Bitwise `& | ^ << >>` with MySQL's 64-bit unsigned semantics:
/// Ints reinterpreted as u64 (negative values are two's complement),
/// everything wraps, NULL propagates, shifts >= 64 yield 0.
pub fn eval_bitop(op: &BinOp, l: &Value, r: &Value) -> SqlResult<Value> {
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::Int(a), Value::Int(b)) = (l, r) else {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("bitwise {op:?} on {l:?} and {r:?}"),
        ));
    };
    let (a, b) = (*a as u64, *b as u64);
    let v = match op {
        BinOp::BitAnd => a & b,
        BinOp::BitOr => a | b,
        BinOp::BitXor => a ^ b,
        // MySQL does not mask the shift count to 6 bits: a shift past
        // the width shifts everything out (0), unlike Rust's checked_shl.
        BinOp::Shl => {
            if b < 64 {
                a.wrapping_shl(b as u32)
            } else {
                0
            }
        }
        BinOp::Shr => {
            if b < 64 {
                a.wrapping_shr(b as u32)
            } else {
                0
            }
        }
        _ => unreachable!("caller pins the op to the bit set"),
    };
    Ok(Value::Int(v as i64))
}

#[cfg(test)]
#[path = "numeric_tests.rs"]
mod tests;
