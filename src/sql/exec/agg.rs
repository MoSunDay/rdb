//! Aggregate evaluation over grouped rows.
//!
//! Aggregates never run inside the plain expression evaluator: before
//! evaluation, every `Agg` node of an expression is substituted by the
//! literal computed from the group's rows ([`substitute_aggs`]); the
//! rewritten tree then evaluates on the group's representative row.

use crate::sql::exec::expr::{bigint_out_of_range, cmp_values, eval};
use crate::sql::exec::expr_decimal::{decimal_to_f64, div_decimal, rescale_decimal};
use crate::sql::exec::scan::FromScope;
use crate::sql::parse::ast::{AggFunc, CorrelatedKind, Expr};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;

/// One evaluation unit: a row (ungrouped query) or a group of rows.
pub struct Unit {
    /// Representative row for non-aggregate expressions (first row of
    /// the group; all-NULL row for an empty global group).
    pub rep: Vec<Value>,
    /// The group's rows aggregated over by aggregate functions.
    pub rows: Vec<Vec<Value>>,
}

/// Evaluate `e` in the unit's context: aggregate nodes consume the
/// group's rows, everything else evaluates on the representative row.
pub fn eval_in_group(e: &Expr, scope: &FromScope, u: &Unit) -> SqlResult<Value> {
    if !has_agg(e) {
        return eval(e, scope, &u.rep);
    }
    eval(&substitute_aggs(e, scope, u)?, scope, &u.rep)
}

/// Replace every Agg node with the literal aggregated over the group.
fn substitute_aggs(e: &Expr, scope: &FromScope, u: &Unit) -> SqlResult<Expr> {
    Ok(match e {
        Expr::Agg { .. } => Expr::Lit(eval_aggregate(e, scope, &u.rows)?),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_aggs(left, scope, u)?),
            op: *op,
            right: Box::new(substitute_aggs(right, scope, u)?),
        },
        Expr::Not(x) => Expr::Not(Box::new(substitute_aggs(x, scope, u)?)),
        Expr::Neg(x) => Expr::Neg(Box::new(substitute_aggs(x, scope, u)?)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(substitute_aggs(expr, scope, u)?),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(substitute_aggs(expr, scope, u)?),
            list: list
                .iter()
                .map(|i| substitute_aggs(i, scope, u))
                .collect::<SqlResult<Vec<_>>>()?,
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(substitute_aggs(expr, scope, u)?),
            low: Box::new(substitute_aggs(low, scope, u)?),
            high: Box::new(substitute_aggs(high, scope, u)?),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(substitute_aggs(expr, scope, u)?),
            pattern: Box::new(substitute_aggs(pattern, scope, u)?),
            negated: *negated,
        },
        // Scalar-function wrappers aggregate through their arguments
        // (`has_agg` already descends these): rewrite each arg so
        // ROUND(SUM(x), 2) / COALESCE(SUM(x), 0) see the computed
        // literal instead of a stray Agg node at Func evaluation.
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|a| substitute_aggs(a, scope, u))
                .collect::<SqlResult<Vec<_>>>()?,
        },
        // CASE branches may aggregate (HAVING-style CASE WHEN COUNT..),
        // so descend; CAST/REGEXP wrap a single expression each.
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| substitute_aggs(o, scope, u).map(Box::new))
                .transpose()?,
            branches: branches
                .iter()
                .map(|(c, t)| Ok((substitute_aggs(c, scope, u)?, substitute_aggs(t, scope, u)?)))
                .collect::<SqlResult<Vec<_>>>()?,
            else_expr: else_expr
                .as_ref()
                .map(|e| substitute_aggs(e, scope, u).map(Box::new))
                .transpose()?,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(substitute_aggs(expr, scope, u)?),
            to: *to,
        },
        // A bound correlated node: only an IN's left side can carry an
        // aggregate of THIS query (`COUNT(*) IN (SELECT ...)`); keys
        // are plain outer columns and cases are values.
        Expr::Correlated { kind, keys, cases } => Expr::Correlated {
            kind: match kind {
                CorrelatedKind::In { lhs, negated } => CorrelatedKind::In {
                    lhs: Box::new(substitute_aggs(lhs, scope, u)?),
                    negated: *negated,
                },
                k => k.clone(),
            },
            keys: keys.clone(),
            cases: cases.clone(),
        },
        Expr::Regexp {
            expr,
            pattern,
            negated,
        } => Expr::Regexp {
            expr: Box::new(substitute_aggs(expr, scope, u)?),
            pattern: Box::new(substitute_aggs(pattern, scope, u)?),
            negated: *negated,
        },
        // Leaves and hoisted subqueries: `has_agg` reports no
        // aggregate of THIS query inside any of these shapes.
        other => other.clone(),
    })
}

/// One aggregate over the group's rows.
fn eval_aggregate(e: &Expr, scope: &FromScope, rows: &[Vec<Value>]) -> SqlResult<Value> {
    let Expr::Agg {
        func,
        arg,
        distinct,
        sep,
    } = e
    else {
        unreachable!("caller checked the shape");
    };
    if matches!(func, AggFunc::Count) && arg.is_none() {
        return Ok(Value::Int(rows.len() as i64)); // COUNT(*)
    }
    let arg = arg.as_deref().expect("COUNT is the only argless agg");
    let mut vals = Vec::with_capacity(rows.len());
    for r in rows {
        vals.push(eval(arg, scope, r)?); // nested aggregates error in eval
    }
    // Aggregates skip NULLs entirely.
    vals.retain(|v| !matches!(v, Value::Null));
    if *distinct {
        dedupe_values(&mut vals);
    }
    match func {
        AggFunc::Count => Ok(Value::Int(vals.len() as i64)),
        AggFunc::Sum => sum_values(&vals),
        AggFunc::Avg => avg_values(&vals),
        AggFunc::Min | AggFunc::Max => Ok(min_max(&vals, matches!(func, AggFunc::Max))),
        AggFunc::GroupConcat => group_concat(&vals, sep.as_deref().unwrap_or(",")),
    }
}

/// GROUP_CONCAT: every value renders in its canonical text form
/// (`func::value_text`, the CONCAT coercion) and joins with the
/// separator (default `,`); an all-NULL group is the empty string,
/// like MySQL (NULL args were already skipped above).
fn group_concat(vals: &[Value], sep: &str) -> SqlResult<Value> {
    let mut out = String::new();
    for v in vals {
        if !out.is_empty() {
            out.push_str(sep);
        }
        out.push_str(&crate::sql::exec::func::value_text(v)?);
    }
    Ok(Value::Str(out))
}

/// Whether every value is exact numeric (Int/Decimal, no Double).
fn all_exact(vals: &[Value]) -> bool {
    !vals.is_empty()
        && !vals.iter().any(|v| matches!(v, Value::Double(_)))
        && vals
            .iter()
            .all(|v| matches!(v, Value::Int(_) | Value::Decimal(..)))
}

fn sum_values(vals: &[Value]) -> SqlResult<Value> {
    if vals.is_empty() {
        return Ok(Value::Null); // SUM over no non-NULL rows is NULL
    }
    if vals.iter().all(|v| matches!(v, Value::Int(_))) {
        // BIGINT SUM has no DECIMAL promotion in this engine, so an
        // overflow is a loud 1690-style error, never a wrapped value.
        let sum = vals.iter().try_fold(0i64, |acc, v| {
            let Value::Int(i) = v else {
                unreachable!("checked")
            };
            acc.checked_add(*i)
        });
        return sum
            .map(Value::Int)
            .ok_or_else(|| bigint_out_of_range("SUM(...)"));
    }
    // Decimal (alone or mixed with Int) sums exactly at the coarser
    // input scale; a Double anywhere keeps the double path below.
    if all_exact(vals) {
        let (m, s) = decimal_sum(vals)?;
        return Ok(Value::Decimal(m, s));
    }
    // Mixed/non-numeric input (e.g. SUM over temporal or string cells)
    // sums only the numeric members; with none, SUM is NULL like AVG
    // (an empty f64 iterator sums to -0.0 in Rust, not 0.0 -- avoid
    // rendering that as a bogus "-0" cell).
    Ok(match vals.iter().filter_map(as_num).reduce(|a, b| a + b) {
        Some(s) => Value::Double(s),
        None => Value::Null,
    })
}

fn avg_values(vals: &[Value]) -> SqlResult<Value> {
    // AVG of exact numerics divides the exact sum by the row count
    // through the decimal division (scale + 4, like MySQL).
    if all_exact(vals) {
        let (m, s) = decimal_sum(vals)?;
        return div_decimal(m, vals.len() as i128, s, 0);
    }
    let nums: Vec<f64> = vals.iter().filter_map(as_num).collect();
    Ok(match nums.len() {
        0 => Value::Null,
        n => Value::Double(nums.iter().sum::<f64>() / n as f64),
    })
}

/// Exact fixed-point sum of Int/Decimal values at the coarser scale;
/// mantissa overflow is a loud error, never a wrap.
fn decimal_sum(vals: &[Value]) -> SqlResult<(i128, u8)> {
    let overflow = || SqlError::new(ErrorCode::NotSupported, "decimal SUM overflow".to_string());
    let scale = vals
        .iter()
        .map(|v| match v {
            Value::Decimal(_, s) => *s,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let mut acc: i128 = 0;
    for v in vals {
        let (m, s) = match v {
            Value::Decimal(m, s) => (*m, *s),
            Value::Int(i) => (i128::from(*i), 0),
            other => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("SUM({other:?})"),
                ))
            }
        };
        acc = acc
            .checked_add(rescale_decimal(m, s, scale).map_err(|_| overflow())?)
            .ok_or_else(overflow)?;
    }
    Ok((acc, scale))
}

fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Double(d) => Some(*d),
        Value::Decimal(m, s) => Some(decimal_to_f64(*m, *s)),
        _ => None,
    }
}

fn min_max(vals: &[Value], want_max: bool) -> Value {
    let Some(first) = vals.first() else {
        return Value::Null;
    };
    let mut best = first.clone();
    for v in &vals[1..] {
        let ord = match cmp_values(v, &best) {
            Ok(o) => o,
            Err(_) => continue, // inhomogeneous group: keep the best so far
        };
        if (want_max && ord.is_gt()) || (!want_max && ord.is_lt()) {
            best = v.clone();
        }
    }
    best
}

fn dedupe_values(vals: &mut Vec<Value>) {
    let mut out: Vec<Value> = Vec::with_capacity(vals.len());
    for v in vals.drain(..) {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    *vals = out;
}

/// Whether an expression contains any aggregate node.
pub fn has_agg(e: &Expr) -> bool {
    match e {
        Expr::Agg { .. } => true,
        Expr::Lit(_) | Expr::Placeholder | Expr::Col { .. } => false,
        Expr::InsertValues(_) => false,
        // Subqueries hoist out before evaluation; their aggregates
        // belong to the inner query, not this one. A bound correlated
        // node's IN left side is evaluated HERE, so its aggregates do
        // count (keys are plain outer columns).
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => false,
        Expr::Correlated { kind, keys, .. } => {
            keys.iter().any(has_agg)
                || matches!(kind, CorrelatedKind::In { lhs, .. } if has_agg(lhs))
        }
        Expr::BinaryOp { left, right, .. } => has_agg(left) || has_agg(right),
        Expr::Not(x) | Expr::Neg(x) => has_agg(x),
        Expr::IsNull { expr, .. } => has_agg(expr),
        Expr::InList { expr, list, .. } => has_agg(expr) || list.iter().any(has_agg),
        Expr::Between {
            expr, low, high, ..
        } => has_agg(expr) || has_agg(low) || has_agg(high),
        Expr::Like { expr, pattern, .. } => has_agg(expr) || has_agg(pattern),
        Expr::Func { args, .. } => args.iter().any(has_agg),
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            operand.as_deref().is_some_and(has_agg)
                || branches.iter().any(|(c, t)| has_agg(c) || has_agg(t))
                || else_expr.as_deref().is_some_and(has_agg)
        }
        Expr::Cast { expr, .. } => has_agg(expr),
        Expr::Regexp { expr, pattern, .. } => has_agg(expr) || has_agg(pattern),
    }
}

#[cfg(test)]
#[path = "agg_tests.rs"]
mod agg_tests;
