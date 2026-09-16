//! Decimal (fixed-point i128) arithmetic and scaling helpers.
//!
//! `Value::Decimal` stores `mantissa * 10^-scale`; everything here is
//! exact i128 math that errors on overflow instead of wrapping.

use crate::sql::parse::ast::BinOp;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{format_decimal, Value, MAX_DECIMAL_SCALE};

/// Exact fixed-point arithmetic for Decimal (and Int) operands. Ints
/// enter as scale-0 decimals; Add/Sub/Mod align to the coarser scale,
/// Mul multiplies mantissae and adds scales; Div ([`div_decimal`])
/// carries the dividend's scale + 4 and rounds half away from zero.
/// Division by zero is NULL, like the int path.
pub(super) fn arith_decimal(op: &BinOp, l: &Value, r: &Value) -> SqlResult<Value> {
    use BinOp::*;
    let (ma, sa) = match l {
        Value::Decimal(m, s) => (*m, *s),
        Value::Int(i) => (i128::from(*i), 0),
        other => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("arithmetic on {other:?} and {r:?}"),
            ))
        }
    };
    let (mb, sb) = match r {
        Value::Decimal(m, s) => (*m, *s),
        Value::Int(i) => (i128::from(*i), 0),
        other => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("arithmetic on {l:?} and {other:?}"),
            ))
        }
    };
    let overflow = || {
        SqlError::new(
            ErrorCode::NotSupported,
            format!("decimal arithmetic overflow: {l:?} {op:?} {r:?}"),
        )
    };
    let align = |m: i128, s: u8, to: u8| rescale_decimal(m, s, to).map_err(|_| overflow());
    Ok(match op {
        Add => {
            let s = sa.max(sb);
            Value::Decimal(
                align(ma, sa, s)?
                    .checked_add(align(mb, sb, s)?)
                    .ok_or_else(overflow)?,
                s,
            )
        }
        Sub => {
            let s = sa.max(sb);
            Value::Decimal(
                align(ma, sa, s)?
                    .checked_sub(align(mb, sb, s)?)
                    .ok_or_else(overflow)?,
                s,
            )
        }
        Mul => {
            let s = sa + sb;
            if s > MAX_DECIMAL_SCALE {
                return Err(overflow());
            }
            Value::Decimal(ma.checked_mul(mb).ok_or_else(overflow)?, s)
        }
        Div => {
            if mb == 0 {
                return Ok(Value::Null);
            }
            div_decimal(ma, mb, sa, sb)?
        }
        Mod => {
            if mb == 0 {
                return Ok(Value::Null);
            }
            let s = sa.max(sb);
            Value::Decimal(
                align(ma, sa, s)?
                    .checked_rem(align(mb, sb, s)?)
                    .ok_or_else(overflow)?,
                s,
            )
        }
        _ => unreachable!("arith guards the operator set"),
    })
}

/// `10^scale` for a decimal scale (1..=10^38, always inside i128).
pub(super) fn pow10(scale: u8) -> i128 {
    debug_assert!(scale <= MAX_DECIMAL_SCALE);
    10i128.pow(u32::from(scale))
}

/// Exact MySQL-style division: the quotient carries the dividend's
/// scale + 4 (`div_precision_increment`) and the remainder rounds half
/// away from zero. Shared by `/` and AVG (which divides the exact sum
/// by the row count); the divisor is the caller's business (NULL for
/// `/`, never zero for AVG).
pub(super) fn div_decimal(ma: i128, mb: i128, sa: u8, sb: u8) -> SqlResult<Value> {
    debug_assert!(mb != 0);
    let overflow = || {
        SqlError::new(
            ErrorCode::NotSupported,
            format!("decimal division overflow: {ma}e-{sa} / {mb}e-{sb}"),
        )
    };
    let t = (sa + 4).min(MAX_DECIMAL_SCALE);
    // `a/b` at scale t is round_half_away(ma * 10^(t-sa+sb) / mb): the
    // pre-scaled dividend rides one checked_mul chain; while it fits
    // i128 the quotient and remainder are exact.
    let lift = u32::from(t) - u32::from(sa) + u32::from(sb);
    if let Some(p) = 10i128.checked_pow(lift) {
        if let Some(d) = ma.checked_mul(p) {
            let (q, r) = (d.checked_div(mb), d.checked_rem(mb));
            if let (Some(q), Some(r)) = (q, r) {
                // |r| >= half the divisor -> one unit away from zero.
                let bump = r != 0 && r.unsigned_abs() * 2 >= mb.unsigned_abs();
                let q = if bump {
                    q.checked_add(if q.is_negative() { -1 } else { 1 })
                } else {
                    Some(q)
                };
                return match q {
                    Some(q) => Ok(Value::Decimal(q, t)),
                    None => Err(overflow()),
                };
            }
            return Err(overflow());
        }
    }
    // TODO(W2.0-exec): fires only when |ma| * 10^(sa+4-sa+sb) exceeds
    // i128 -- a 34+ significant-digit dividend or a wide-divisor scale
    // (e.g. 1e33 / 1e-5); exact long division over a wider
    // intermediate would be needed. The f64 quotient coarsens past
    // 2^53 significands.
    Ok(Value::Double(
        decimal_to_f64(ma, sa) / decimal_to_f64(mb, sb),
    ))
}

/// Store shape for a DECIMAL(precision, scale) column: rescale to the
/// column scale (rounding half away from zero), then bound the
/// mantissa by the declared width -- |m| >= 10^precision means the
/// integer part needs more than precision-scale digits (MySQL 1264).
pub(super) fn fit_column(
    m: i128,
    from: u8,
    precision: u8,
    scale: u8,
    shown: &str,
) -> SqlResult<Value> {
    let m = rescale_decimal(m, from, scale)?;
    if m.unsigned_abs() >= pow10(precision) as u128 {
        return Err(decimal_out_of_range(shown, scale));
    }
    Ok(Value::Decimal(m, scale))
}

/// MySQL 1264-style out-of-range error for decimal coercions.
pub(super) fn decimal_out_of_range(text: &str, scale: u8) -> SqlError {
    SqlError::new(
        ErrorCode::WrongValue,
        format!("Out of range value for DECIMAL({scale}): '{text}'"),
    )
}

/// Approximate a decimal as f64 (mantissa / 10^scale). Beyond ~2^53
/// significands this coarsens; exact paths replace it in W2.0-exec.
pub(super) fn decimal_to_f64(m: i128, scale: u8) -> f64 {
    m as f64 / pow10(scale.min(38)) as f64
}

/// Re-scale a decimal mantissa from `from` to `to` digits behind the
/// point. Scaling up must stay inside i128; scaling down rounds half
/// away from zero (MySQL cast style). Errors on overflow.
pub(super) fn rescale_decimal(m: i128, from: u8, to: u8) -> SqlResult<i128> {
    match to.cmp(&from) {
        std::cmp::Ordering::Equal => Ok(m),
        std::cmp::Ordering::Greater => m
            .checked_mul(pow10(to - from))
            .ok_or_else(|| decimal_out_of_range(&format_decimal(m, from), to)),
        std::cmp::Ordering::Less => {
            let d = pow10(from - to);
            let q = m / d;
            let r = m % d;
            // |remainder| >= half the divisor -> one more unit away
            // from zero (i128::MIN has headroom: d >= 10 here).
            if r != 0 && r.unsigned_abs() * 2 >= d.unsigned_abs() {
                Ok(if m < 0 { q - 1 } else { q + 1 })
            } else {
                Ok(q)
            }
        }
    }
}
