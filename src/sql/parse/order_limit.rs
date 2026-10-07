//! ORDER BY / GROUP BY key resolution and LIMIT / OFFSET translation.
//!
//! Split out of `translate.rs` (file-size budget): everything here
//! needs context the generic expression translator lacks -- the select
//! list (ordinal positions, aliases) or the parameter-binding path
//! (`LIMIT ?` rides through `bind_placeholders` and is coerced to u64
//! at execution time).

use sqlparser::ast::Expr as SqlExpr;

use crate::sql::parse::ast::{Expr, LimitValue};
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::schema::Value;

// --------------------------------------------------------- LIMIT / OFFSET

fn bad_limit() -> SqlError {
    SqlError::parse("LIMIT must be a non-negative integer")
}

/// One LIMIT / OFFSET operand: numeric literal, or a bare `?`
/// placeholder carried to bind time.
fn translate_limit_operand(e: &SqlExpr) -> SqlResult<LimitValue> {
    let SqlExpr::Value(v) = e else {
        return Err(bad_limit());
    };
    match &v.value {
        sqlparser::ast::Value::Number(n, _) => n
            .parse::<u64>()
            .map(LimitValue::Const)
            .map_err(|_| bad_limit()),
        sqlparser::ast::Value::Placeholder(p) if p == "?" => {
            Ok(LimitValue::Param(Box::new(Expr::Placeholder)))
        }
        _ => Err(bad_limit()),
    }
}

/// LIMIT position of a SELECT (`None` when the clause is absent).
pub(crate) fn translate_limit_value(e: &Option<SqlExpr>) -> SqlResult<Option<LimitValue>> {
    e.as_ref().map(translate_limit_operand).transpose()
}

/// OFFSET operand of a SELECT.
pub(crate) fn translate_offset_value(e: &SqlExpr) -> SqlResult<LimitValue> {
    translate_limit_operand(e)
}

/// Coerce a (possibly bound) LIMIT/OFFSET to u64. Called once per
/// execution site; a bound parameter must be a non-negative integer,
/// exactly like a numeric literal at parse time.
pub fn limit_u64(v: &LimitValue) -> SqlResult<u64> {
    match v {
        LimitValue::Const(n) => Ok(*n),
        LimitValue::Param(e) => match e.as_ref() {
            Expr::Lit(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
            _ => Err(bad_limit()),
        },
    }
}

/// EXPLAIN rendering: constants as numbers, parameters as their
/// placeholder spelling (EXPLAIN output is never parameter-bound).
pub fn limit_display(v: &LimitValue) -> String {
    match v {
        LimitValue::Const(n) => n.to_string(),
        LimitValue::Param(e) => match e.as_ref() {
            Expr::Placeholder => "?".to_string(),
            Expr::Lit(Value::Int(n)) => n.to_string(),
            _ => "(param)".to_string(),
        },
    }
}

// ------------------------------------------------- ORDER BY / GROUP BY keys
