//! Hash equi-join for INNER joins (03-perf-study gap #4): the nested
//! loop in `scan::join_sources` is O(L*R); when the join condition is
//! a conjunction of column=column equalities between the two sides,
//! build a hash table over the right side and probe with the left,
//! keeping every non-equi conjunct as a per-candidate filter. Rows are
//! emitted in exactly the nested loop's order (build order within one
//! left row), so plans are interchangeable with no observable change.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::Hasher;

use crate::sql::exec::expr::{eval, truthy, ColumnScope};
use crate::sql::exec::scan::FromScope;
use crate::sql::exec::scan::Source;
use crate::sql::parse::ast::{BinOp, Expr};
use crate::sql::parse::error::SqlResult;
use crate::sql::storage::schema::{SqlType, Value};

/// One hashable equality: positions of the matched columns, LOCAL to
/// each side's row slice (scope offsets are not needed here).
struct EquiPair {
    left: usize,
    right: usize,
}

/// Everything the probe loop needs: equality positions, the residual
/// conjunction (non-equi ON terms, re-ANDed), and the merged scope the
/// residual evaluates against.
pub struct HashJoinPlan {
    pairs: Vec<EquiPair>,
    residual: Option<Expr>,
    scope: FromScope,
}

/// Try to plan a hash join; `None` falls back to the nested loop.
/// `scope` is the already-merged combined scope (residual offsets are
/// combined-row offsets; `using` names are resolved against each side
/// to get the local positions).
pub fn try_plan(
    l: &FromScope,
    r: &FromScope,
    on: Option<&Expr>,
    using: &[String],
    scope: FromScope,
) -> Option<HashJoinPlan> {
    let mut pairs = Vec::new();
    let mut residuals: Vec<Expr> = Vec::new();
    match on {
        Some(cond) => {
            for c in flatten_and(cond) {
                match equi_pair(&c, l, r) {
                    Some(p) => pairs.push(p),
                    None => residuals.push(c),
                }
            }
        }
        None => {
            // USING(a, b): every named column must exist on both sides.
            for name in using {
                let left = l.resolve(None, name)?;
                let right = r.resolve(None, name)?;
                pairs.push(EquiPair { left, right });
            }
        }
    }
    if pairs.is_empty() {
        return None;
    }
    if pairs
        .iter()
        .any(|p| !matches!((l.type_at(p.left), r.type_at(p.right)), (Some(a), Some(b)) if hashable(a, b)))
    {
        return None;
    }
    let residual = residuals.into_iter().reduce(|acc, e| Expr::BinaryOp {
        left: Box::new(acc),
        op: BinOp::And,
        right: Box::new(e),
    });
    Some(HashJoinPlan {
        pairs,
        residual,
        scope,
    })
}

/// Same-declared-type columns whose value variants encode canonically;
/// Decimal (scale-dependent equality) falls back to the nested loop.
fn hashable(a: SqlType, b: SqlType) -> bool {
    a == b && !matches!(a, SqlType::Decimal { .. })
}

fn flatten_and(e: &Expr) -> Vec<Expr> {
    match e {
        Expr::BinaryOp {
            left,
            op: BinOp::And,
            right,
        } => {
            let mut out = flatten_and(left);
            out.extend(flatten_and(right));
            out
        }
        other => vec![other.clone()],
    }
}

/// `col = col` with the two columns on opposite sides (positions local
/// to that side's slice). Ambiguous or same-side references are not
/// equi pairs -- they stay in the residual filter.
fn equi_pair(e: &Expr, l: &FromScope, r: &FromScope) -> Option<EquiPair> {
    let Expr::BinaryOp {
        left,
        op: BinOp::Eq,
        right,
    } = e
    else {
        return None;
    };
    let (lc, rc) = (as_col(left)?, as_col(right)?);
    if let (Some(lp), Some(rp)) = (resolve_one(l, lc), resolve_one(r, rc)) {
        return Some(EquiPair {
            left: lp,
            right: rp,
        });
    }
    if let (Some(lp), Some(rp)) = (resolve_one(l, rc), resolve_one(r, lc)) {
        return Some(EquiPair {
            left: lp,
            right: rp,
        });
    }
    None
}

fn as_col(e: &Expr) -> Option<(Option<&str>, &str)> {
    match e {
        Expr::Col { table, name } => Some((table.as_deref(), name.as_str())),
        _ => None,
    }
}

/// Resolve only when the name is unambiguous within one scope.
fn resolve_one(scope: &FromScope, (table, name): (Option<&str>, &str)) -> Option<usize> {
    let mut hits = scope.sides.iter().filter(|s| match table {
        Some(t) => s.qualifier.eq_ignore_ascii_case(t) || s.table.eq_ignore_ascii_case(t),
        None => true,
    });
    let side = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    side.columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
}

/// Canonical bytes per value variant; `None` (NULL or a variant the
/// pair's types should not produce) never matches, matching SQL NULL
/// join-key semantics.
fn key_bytes(v: &Value, out: &mut Vec<u8>) -> bool {
    match v {
        Value::Int(i) => {
            out.extend_from_slice(&i.to_be_bytes());
            true
        }
        Value::Double(d) => {
            out.extend_from_slice(&d.to_bits().to_be_bytes());
            true
        }
        Value::Str(s) => {
            out.extend_from_slice(s.as_bytes());
            true
        }
        Value::Bytes(b) => {
            out.extend_from_slice(b);
            true
        }
        Value::Vector(xs) => {
            for x in xs {
                out.extend_from_slice(&x.to_bits().to_be_bytes());
            }
            true
        }
        _ => false,
    }
}

fn row_hash(row: &[Value], at: &[usize]) -> Option<u64> {
    let mut buf = Vec::new();
    for &i in at {
        if !key_bytes(row.get(i)?, &mut buf) {
            return None;
        }
    }
    let mut h = DefaultHasher::new();
    h.write(&buf);
    Some(h.finish())
}

/// Build over the right side, probe with the left. Candidate rows are
/// re-checked with `cmp_values` semantics so a hash collision can
/// never produce a false match.
pub fn execute(l: &Source, r: &Source, plan: &HashJoinPlan) -> SqlResult<Vec<Vec<Value>>> {
    let rpos: Vec<usize> = plan.pairs.iter().map(|p| p.right).collect();
    let lpos: Vec<usize> = plan.pairs.iter().map(|p| p.left).collect();
    let mut table: HashMap<u64, Vec<usize>> = HashMap::with_capacity(r.rows.len());
    for (i, rr) in r.rows.iter().enumerate() {
        if let Some(h) = row_hash(rr, &rpos) {
            table.entry(h).or_default().push(i);
        }
    }
    let keys_equal = |lr: &[Value], rr: &[Value]| -> bool {
        plan.pairs.iter().all(|p| {
            matches!(
                crate::sql::exec::expr::cmp_values(&lr[p.left], &rr[p.right]),
                Ok(std::cmp::Ordering::Equal)
            )
        })
    };
    let mut rows = Vec::with_capacity(l.rows.len());
    for lr in &l.rows {
        let Some(h) = row_hash(lr, &lpos) else {
            continue;
        };
        let Some(cands) = table.get(&h) else { continue };
        for &i in cands {
            let rr = &r.rows[i];
            if !keys_equal(lr, rr) {
                continue;
            }
            let mut combined = lr.clone();
            combined.extend_from_slice(rr);
            if let Some(f) = &plan.residual {
                if !truthy(&eval(f, &plan.scope, &combined)?)? {
                    continue;
                }
            }
            rows.push(combined);
        }
    }
    Ok(rows)
}

/// Entry point from `scan::join_sources`: hash-join an INNER join when
/// plannable, else signal fallback. The merged scope is rebuilt exactly
/// like the nested-loop path does (INNER never null-extends, so
/// nullability and key_pos carry over verbatim).
pub fn try_join(
    l: &Source,
    r: &Source,
    on: Option<&Expr>,
    using: &[String],
) -> SqlResult<Option<Source>> {
    let mut scope = l.scope.clone();
    let left_width = l.scope.row_width();
    for mut side in r.scope.clone().sides {
        side.offset += left_width;
        scope.sides.push(side);
    }
    match try_plan(&l.scope, &r.scope, on, using, scope) {
        Some(plan) => {
            let rows = execute(l, r, &plan)?;
            Ok(Some(Source {
                scope: plan.scope,
                rows,
            }))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
#[path = "join_hash_tests.rs"]
mod tests;
