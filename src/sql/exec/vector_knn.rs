//! Exact nearest-neighbor fast path for the D11 KNN shape:
//! `SELECT ... FROM t [WHERE ...] ORDER BY DISTANCE(col, <constant>[,
//! 'metric']) LIMIT k [OFFSET o]` -- full scan plus a bounded top-k
//! heap (O(N log k)), ASC only, projection evaluated for the emitted
//! rows alone.
//!
//! [`match_knn`] is the single pure shape matcher: the executor and
//! the EXPLAIN headline both call it, so the explained plan and the
//! executed plan cannot drift. Every shape it declines -- DESC,
//! several keys, GROUP BY/DISTINCT/HAVING, aggregates, a missing
//! LIMIT, both or neither vector slot a column, a metric that reads
//! the row -- falls back to the generic sort pipeline, which is
//! correct for the same expression (DISTANCE answers a plain Double).
//!
//! Result parity with the generic path: same distance math (the
//! scalar-function entry in `func::vector`), NULL distances sort
//! first with scan-order ties (the bounded key is (distance, row
//! index), mirroring the stable sort), and coercion/metric/dimension
//! errors carry the same wording. One deliberate difference: the
//! projection is evaluated only for emitted rows, so a projection
//! error on a non-winning row surfaces on the generic path only.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::sql::exec::agg::has_agg;
use crate::sql::exec::expr::eval;
use crate::sql::exec::func::{distance, metric_arg, vector_arg, Metric};
use crate::sql::exec::render;
use crate::sql::exec::scan::{FromScope, Source};
use crate::sql::exec::select::filter_rows;
use crate::sql::exec::ColMeta;
use crate::sql::parse::ast::{Expr, LimitValue, Query, SelectItem};
use crate::sql::parse::error::SqlResult;
use crate::sql::parse::order_limit::limit_display;
use crate::sql::parse::order_limit::limit_u64;
use crate::sql::storage::schema::Value;

/// A recognized KNN shape: borrowed pieces of the query, one per
/// matched clause. `swapped` marks the column sitting in the SECOND
/// DISTANCE slot (the plan accepts either operand position).
pub(crate) struct KnnShape<'a> {
    /// Column operand: evaluated per row.
    pub col: &'a Expr,
    /// Row-constant query vector operand (a literal, or e.g.
    /// STRING_TO_VECTOR('[..]')).
    pub query: &'a Expr,
    /// Row-constant metric operand.
    pub metric: &'a Expr,
    /// Column in the second slot (the D10 mismatch error names the
    /// slots in their written order, so the swap must be honored).
    pub swapped: bool,
    pub limit: &'a LimitValue,
}

/// Match the D11 KNN shape against a whole query. Pure AST inspection
/// -- no scope, no storage -- so it answers identically at EXPLAIN
/// time and at execution time.
pub(crate) fn match_knn(q: &Query) -> Option<KnnShape<'_>> {
    if q.distinct
        || !q.group_by.is_empty()
        || q.having.is_some()
        || q.limit.is_none()
        || q.items
            .iter()
            .any(|i| matches!(i, SelectItem::Expr { expr, .. } if has_agg(expr)))
    {
        return None;
    }
    let [key] = &q.order_by[..] else {
        return None; // several keys (or none): generic sort territory
    };
    if !key.asc {
        return None; // DESC = farthest first: not a KNN, generic sort
    }
    let Expr::Func { name, args } = &key.expr else {
        return None;
    };
    // The plan's signature is DISTANCE(v1, v2, metric): a 2-arg call
    // never reaches execution (parse-time arity table), but the
    // matcher still declines anything but the spelled form.
    if !name.eq_ignore_ascii_case("distance") || args.len() != 3 {
        return None;
    }
    let (col, query, swapped) = match (&args[0], &args[1]) {
        (a, b) if is_column(a) && is_row_constant(b) => (a, b, false),
        (a, b) if is_column(b) && is_row_constant(a) => (b, a, true),
        _ => return None, // two columns, two constants, or worse
    };
    if !is_row_constant(&args[2]) {
        return None;
    }
    Some(KnnShape {
        col,
        query,
        metric: &args[2],
        swapped,
        limit: q.limit.as_ref()?,
    })
}

/// A plain column reference (the per-row operand).
fn is_column(e: &Expr) -> bool {
    matches!(e, Expr::Col { .. })
}

/// Whether an expression reads nothing from the row: literals and
/// pure functions/arithmetic over literals. Bound parameters are
/// literals by execution time; subqueries were rewritten away.
fn is_row_constant(e: &Expr) -> bool {
    match e {
        Expr::Lit(_) => true,
        Expr::Func { args, .. } => args.iter().all(is_row_constant),
        Expr::Neg(x) | Expr::Not(x) => is_row_constant(x),
        Expr::BinaryOp { left, right, .. } => is_row_constant(left) && is_row_constant(right),
        Expr::Cast { expr, .. } => is_row_constant(expr),
        _ => false,
    }
}

/// Run the recognized shape over the materialized source. `None` (the
/// caller's fallback signal) only ever comes from the matcher; every
/// evaluation error is the same error the generic path would raise.
/// Exact-KNN projection outcome: result column shape + rows.
type KnnOutcome = (Vec<ColMeta>, Vec<Vec<Value>>);

pub(crate) fn try_execute(
    q: &Query,
    src: &Source,
    items: &[(Expr, ColMeta)],
) -> SqlResult<Option<KnnOutcome>> {
    let Some(shape) = match_knn(q) else {
        return Ok(None);
    };
    let scope = &src.scope;
    let rows = filter_rows(&src.rows, scope, q.filter.as_ref())?;

    // Query-side constants resolve ONCE against an empty row (a row
    // constant never reads it). NULL operands make every distance
    // NULL, exactly like the scalar call's three-valued logic.
    let query_val = eval(shape.query, scope, &[])?;
    let metric_val = eval(shape.metric, scope, &[])?;
    let metric = metric_arg(&metric_val)?;
    let query_vec = match &query_val {
        Value::Null => None,
        v => Some(vector_arg("distance", v)?),
    };

    let take = limit_u64(shape.limit)? as usize;
    let offset = limit_u64(&q.offset)? as usize;
    let cap = offset.saturating_add(take);

    let mut heap = BinaryHeap::with_capacity(cap.min(rows.len()));
    for (idx, row) in rows.iter().enumerate() {
        let key = Hit(
            row_distance(&shape, scope, row, query_vec.as_deref(), metric)?,
            idx as u64,
        );
        push_bounded(&mut heap, cap, key);
    }

    let mut winners: Vec<Hit> = heap.into_iter().collect();
    winners.sort();
    let columns: Vec<ColMeta> = items.iter().map(|(_, m)| m.clone()).collect();
    let mut out = Vec::with_capacity(winners.len().saturating_sub(offset).min(take));
    for hit in winners.into_iter().skip(offset).take(take) {
        let row = &rows[hit.1 as usize];
        let mut projected = Vec::with_capacity(items.len());
        for (e, _) in items {
            projected.push(eval(e, scope, row)?);
        }
        out.push(projected);
    }
    Ok(Some((columns, out)))
}

/// Distance of one row: the column operand evaluated against it, then
/// the scalar call's exact value-level semantics (NULL, coercion,
/// metric math, error wording) with the operand order as written.
/// NULL rides as negative infinity so it sorts first, like
/// `cmp_null_first`.
fn row_distance(
    shape: &KnnShape<'_>,
    scope: &FromScope,
    row: &[Value],
    query_vec: Option<&[f32]>,
    metric: Option<Metric>,
) -> SqlResult<f64> {
    let (Some(qv), Some(m)) = (query_vec, metric) else {
        return Ok(f64::NEG_INFINITY); // NULL query/metric: NULL distance
    };
    let col_val = eval(shape.col, scope, row)?;
    if matches!(col_val, Value::Null) {
        return Ok(f64::NEG_INFINITY);
    }
    let cv = vector_arg("distance", &col_val)?;
    let (a, b) = if shape.swapped {
        (qv, &cv[..])
    } else {
        (&cv[..], qv)
    };
    distance(a, b, m)
}

/// Bounded max-heap push: below capacity everything is kept; once
/// full, only a strictly better key displaces the worst kept entry.
/// Equal keys keep the earlier row -- stable-sort parity.
fn push_bounded(heap: &mut BinaryHeap<Hit>, cap: usize, key: Hit) {
    if cap == 0 {
        return;
    }
    if heap.len() < cap {
        heap.push(key);
    } else if heap.peek().is_some_and(|worst| key < *worst) {
        heap.pop();
        heap.push(key);
    }
}

/// One (distance, row index) collector key. Totally ordered: a NULL
/// distance rides as -infinity and real distances are always finite
/// (f32 elements widened to f64 cannot overflow any metric sum), so
/// the NaN arm of float ordering is unreachable and follows the
/// `unwrap_or(Equal)` convention of `cmp_values`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Hit(f64, u64);

// Eq by hand: the marker needs no field support, and `cmp` below is
// total by construction (no NaN ever enters the heap).
impl Eq for Hit {}

impl Ord for Hit {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .partial_cmp(&other.0)
            .unwrap_or(Ordering::Equal)
            .then(self.1.cmp(&other.1))
    }
}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// EXPLAIN headline for the recognized shape: the scan verdict is
/// replaced by the collector's own node. `None` when the matcher
/// declines (the caller keeps its SeqScan/IndexScan/gather line).
pub(crate) fn explain_line(q: &Query) -> Option<String> {
    let shape = match_knn(q)?;
    let metric = match shape.metric {
        Expr::Lit(Value::Str(s)) => s.to_ascii_lowercase(),
        _ => "?".to_string(),
    };
    Some(format!(
        "vector_knn_scan {} top {} by {}",
        render::from_display(&q.from),
        limit_display(shape.limit),
        metric
    ))
}

#[cfg(test)]
#[path = "vector_knn_tests.rs"]
mod tests;
