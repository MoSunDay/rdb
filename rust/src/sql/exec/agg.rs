//! Aggregate evaluation over grouped rows.
//!
//! Aggregates never run inside the plain expression evaluator: before
//! evaluation, every `Agg` node of an expression is substituted by the
//! literal computed from the group's rows ([`substitute_aggs`]); the
//! rewritten tree then evaluates on the group's representative row.

use crate::sql::exec::expr::{cmp_values, eval};
use crate::sql::exec::scan::FromScope;
use crate::sql::parse::ast::{AggFunc, Expr};
use crate::sql::parse::error::SqlResult;
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
        // Leaves (and Func, whose eval is an unsupported error anyway).
        other => other.clone(),
    })
}

/// One aggregate over the group's rows.
fn eval_aggregate(e: &Expr, scope: &FromScope, rows: &[Vec<Value>]) -> SqlResult<Value> {
    let Expr::Agg {
        func,
        arg,
        distinct,
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
        AggFunc::Sum => Ok(sum_values(&vals)),
        AggFunc::Avg => Ok(avg_values(&vals)),
        AggFunc::Min | AggFunc::Max => Ok(min_max(&vals, matches!(func, AggFunc::Max))),
    }
}

fn sum_values(vals: &[Value]) -> Value {
    if vals.is_empty() {
        return Value::Null; // SUM over no non-NULL rows is NULL
    }
    if vals.iter().all(|v| matches!(v, Value::Int(_))) {
        // Overflow-promotion is out of scope for v1: i64 wrapping sum.
        let sum = vals.iter().fold(0i64, |acc, v| {
            let Value::Int(i) = v else {
                unreachable!("checked")
            };
            acc.wrapping_add(*i)
        });
        return Value::Int(sum);
    }
    Value::Double(vals.iter().filter_map(as_num).sum())
}

fn avg_values(vals: &[Value]) -> Value {
    let nums: Vec<f64> = vals.iter().filter_map(as_num).collect();
    match nums.len() {
        0 => Value::Null,
        n => Value::Double(nums.iter().sum::<f64>() / n as f64),
    }
}

fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Double(d) => Some(*d),
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
        Expr::BinaryOp { left, right, .. } => has_agg(left) || has_agg(right),
        Expr::Not(x) | Expr::Neg(x) => has_agg(x),
        Expr::IsNull { expr, .. } => has_agg(expr),
        Expr::InList { expr, list, .. } => has_agg(expr) || list.iter().any(has_agg),
        Expr::Between {
            expr, low, high, ..
        } => has_agg(expr) || has_agg(low) || has_agg(high),
        Expr::Like { expr, pattern, .. } => has_agg(expr) || has_agg(pattern),
        Expr::Func { args, .. } => args.iter().any(has_agg),
    }
}
