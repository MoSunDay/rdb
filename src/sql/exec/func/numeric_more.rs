//! Numeric miscellany: MOD (delegated to the operator path), POW/
//! POWER, SQRT, SIGN and the GREATEST/LEAST reducers. Split from
//! `numeric.rs` at the file budget.

use crate::sql::exec::expr::{as_double, cmp_values, eval_binop};
use crate::sql::exec::expr_decimal::rescale_decimal;
use crate::sql::parse::ast::BinOp;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;

use super::wrong_param_count;

/// Evaluate one numeric_more function; `None` = name not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "mod" => Some(mod_(name, args)),
        "pow" | "power" => Some(pow(name, args)),
        "sqrt" => Some(sqrt(name, args)),
        "sign" => Some(sign(name, args)),
        "greatest" => Some(greatest_least(name, args, true)),
        "least" => Some(greatest_least(name, args, false)),
        _ => None,
    }
}

/// MOD(a, b) is exactly the `%` operator: NULL on b = 0, sign follows
/// the dividend, exact on Decimal.
fn mod_(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [a, b] = args else {
        return Err(wrong_param_count(name));
    };
    eval_binop(&BinOp::Mod, a, b)
}

/// POW/POWER -> DOUBLE (MySQL never keeps these exact). The exponent
/// is bounded to [-30, 30]: past that a double answer is not
/// trustworthy (2^10000 overflows to inf), so the call is NULL rather
/// than a wrong or infinite value -- the ±30 clamp keeps every
/// in-bound answer exact in double. Within the bound a still
/// non-finite result (huge base, 0 to a negative power) is NULL too.
fn pow(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [a, b] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let (a, b) = (as_double(a)?, as_double(b)?);
    const EXPONENT_BOUND: f64 = 30.0;
    if !(-EXPONENT_BOUND..=EXPONENT_BOUND).contains(&b) {
        return Ok(Value::Null);
    }
    let v = a.powf(b);
    Ok(if v.is_finite() {
        Value::Double(v)
    } else {
        Value::Null
    })
}

/// SQRT: negative input is NULL (MySQL), not an error.
fn sqrt(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [x] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(x, Value::Null) {
        return Ok(Value::Null);
    }
    let d = as_double(x)?;
    Ok(if d < 0.0 {
        Value::Null
    } else {
        Value::Double(d.sqrt())
    })
}

fn sign(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [x] = args else {
        return Err(wrong_param_count(name));
    };
    let neg = match x {
        Value::Null => return Ok(Value::Null),
        Value::Int(i) => *i < 0,
        Value::Double(d) => *d < 0.0,
        Value::Decimal(m, _) => *m < 0,
        other => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("sign({other:?})"),
            ))
        }
    };
    let zero = match x {
        Value::Int(i) => *i == 0,
        Value::Double(d) => *d == 0.0,
        Value::Decimal(m, _) => *m == 0,
        _ => unreachable!("guard pins the numeric shapes"),
    };
    Ok(Value::Int(match (neg, zero) {
        (_, true) => 0,
        (true, _) => -1,
        (false, _) => 1,
    }))
}

/// GREATEST/LEAST: any NULL argument is NULL. All-string groups
/// compare byte-wise (no collation); numeric groups pick the extreme
/// and re-render it at the widest input type (Double > Decimal > Int).
/// A string/number mix is a loud reject (no implicit cross-domain
/// coercion exists).
fn greatest_least(name: &str, args: &[Value], want_max: bool) -> SqlResult<Value> {
    if args.is_empty() {
        return Err(wrong_param_count(name));
    }
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Value::Null);
    }
    let all_text = args
        .iter()
        .all(|v| matches!(v, Value::Str(_) | Value::Bytes(_)));
    let mut best = args[0].clone();
    for v in &args[1..] {
        let ord = cmp_values(v, &best).map_err(|_| {
            SqlError::new(
                ErrorCode::NotSupported,
                format!("{name} on mixed {best:?} / {v:?} (v1: all-string or all-numeric)"),
            )
        })?;
        if (want_max && ord.is_gt()) || (!want_max && ord.is_lt()) {
            best = v.clone();
        }
    }
    if all_text {
        return Ok(best);
    }
    widen(args, best)
}

/// Re-express the winner at the widest input type: Double wins over
/// Decimal (coarser scale of the decimal args), Decimal over Int.
fn widen(args: &[Value], best: Value) -> SqlResult<Value> {
    if args.iter().any(|v| matches!(v, Value::Double(_))) {
        return Ok(Value::Double(as_double(&best)?));
    }
    let scale = args
        .iter()
        .filter_map(|v| match v {
            Value::Decimal(_, s) => Some(*s),
            _ => None,
        })
        .max();
    match (best, scale) {
        (Value::Int(i), Some(s)) => rescale_decimal(i128::from(i), 0, s)
            .map(|m| Value::Decimal(m, s))
            .map_err(|_| {
                SqlError::new(ErrorCode::WrongValue, "GREATEST/LEAST overflow".to_string())
            }),
        // A decimal winner is already at the group's coarsest scale
        // (the max includes it); no decimal input keeps Int as-is.
        (best, _) => Ok(best),
    }
}

#[cfg(test)]
#[path = "numeric_more_tests.rs"]
mod tests;
