//! Compound query execution: CTEs (WITH), set operations
//! (UNION / INTERSECT / EXCEPT, each `[ALL]`), and the trailing
//! ORDER BY / LIMIT that belong to the whole compound.
//!
//! Each operand materializes into a [`Relation`] (SELECTs run through
//! the normal select pipeline -- including cluster scatter-gather --
//! at the caller's snapshot timestamp, so a compound never mixes read
//! points). Every operator validates column arity, widens numeric
//! columns, and adopts the left operand's column names, like MySQL.
//! DISTINCT variants dedup (NULLs equal); ALL variants do multiset
//! arithmetic (min / subtraction / concatenation). Both operands are
//! fully materialized on this node before combining, so nothing here
//! assumes streaming across cluster nodes.

use std::sync::Arc;

use crate::sql::exec::expr::{cmp_values, eval};
use crate::sql::exec::relation::{relation_of, CteScope, Relation};
use crate::sql::exec::select;
use crate::sql::exec::subquery::SubqCtx;
use crate::sql::parse::ast::{CompoundQuery, LimitValue, OrderKey, QueryBody, SetOp};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::parse::order_limit::limit_u64;
use crate::sql::storage::schema::{SqlType, Value};
use crate::sql::tx::Txn;
use crate::state::Shared;

/// Execute a compound query into a materialized relation.
pub async fn run_compound(
    shared: &Shared,
    read_ts: u64,
    txn: Option<&Txn>,
    cq: &CompoundQuery,
    outer: &CteScope,
) -> SqlResult<Relation> {
    let mut scope = outer.clone();
    for cte in &cq.ctes {
        let mut rel = Box::pin(run_compound(shared, read_ts, txn, &cte.query, &scope)).await?;
        rel.rename_columns(&cte.column_aliases)?;
        scope.push(cte.name.clone(), rel);
    }
    let ctx = SubqCtx {
        shared,
        read_ts,
        txn,
        ctes: &scope,
        outer: None,
    };
    let mut rel = Box::pin(eval_body(shared, read_ts, txn, &cq.body, &ctx)).await?;
    apply_tail(&mut rel, &cq.order_by, cq.limit.as_ref(), &cq.offset)?;
    Ok(rel)
}

/// Entry from the statement dispatcher: resolves the snapshot like
/// `select::run` (txn read_ts inside a transaction, oracle `now()`
/// for autocommit) and returns the rowset outcome.
pub async fn run_statement(
    shared: &Shared,
    sess: &crate::sql::exec::SqlSession,
    cq: &CompoundQuery,
) -> SqlResult<(Vec<crate::sql::exec::ColMeta>, Vec<Vec<Value>>)> {
    let scope = CteScope::default();
    let rel = match sess.txn.as_ref() {
        Some(t) => run_compound(shared, t.read_ts, Some(t), cq, &scope).await?,
        None => run_compound(shared, shared.sql_ts.now(), None, cq, &scope).await?,
    };
    Ok((
        rel.columns.clone(),
        Arc::try_unwrap(rel.rows).unwrap_or_else(|a| (*a).clone()),
    ))
}

async fn eval_body(
    shared: &Shared,
    read_ts: u64,
    txn: Option<&Txn>,
    body: &QueryBody,
    ctx: &SubqCtx<'_>,
) -> SqlResult<Relation> {
    match body {
        QueryBody::Select(q) => {
            // run_at itself hoists subqueries (single choke point).
            let (columns, rows) = select::run_at(shared, read_ts, txn, q, ctx.ctes).await?;
            Ok(relation_of(columns, rows))
        }
        QueryBody::Nested(inner) => {
            Box::pin(run_compound(shared, read_ts, txn, inner, ctx.ctes)).await
        }
        QueryBody::SetOp {
            op,
            left,
            right,
            all,
        } => {
            let l = Box::pin(eval_body(shared, read_ts, txn, left, ctx)).await?;
            let r = Box::pin(eval_body(shared, read_ts, txn, right, ctx)).await?;
            merge_relations(*op, l, r, *all)
        }
    }
}

/// Set-operation semantics shared by UNION / INTERSECT / EXCEPT:
/// arity must match (the error names the operator), numeric columns
/// widen (INT|DOUBLE -> DOUBLE), and each side's cells normalize to
/// the merged column types BEFORE rows combine, so row equality sees
/// `1 = 1.0` and Date = midnight DATETIME. Column names come from
/// the left operand, like MySQL.
fn merge_relations(op: SetOp, l: Relation, r: Relation, all: bool) -> SqlResult<Relation> {
    if l.columns.len() != r.columns.len() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "{op} operands yield different column counts ({} vs {})",
                l.columns.len(),
                r.columns.len()
            ),
        ));
    }
    let columns = l
        .columns
        .iter()
        .zip(&r.columns)
        .map(|(a, b)| {
            let sql_type = widen(a.sql_type, b.sql_type)?;
            // Either operand may produce NULL for this position, and a
            // merged column is never a single table's key.
            Ok(crate::sql::exec::ColMeta {
                table: a.table.clone(),
                name: a.name.clone(),
                sql_type,
                nullable: a.nullable || b.nullable,
                primary: false,
            })
        })
        .collect::<SqlResult<Vec<_>>>()?;
    let mut lrows = l.rows.as_slice().to_vec();
    let mut rrows = r.rows.as_slice().to_vec();
    widen_cells(&columns, &mut lrows)?;
    widen_cells(&columns, &mut rrows)?;
    let rows = combine_rows(op, all, lrows, rrows);
    Ok(relation_of(columns, rows))
}

/// Row combination per operator and quantifier. DISTINCT variants
/// return sorted unique rows (same observable order UNION DISTINCT
/// has always had); ALL variants keep the left operand's row order
/// (UNION ALL concatenates, INTERSECT ALL / EXCEPT ALL emit their
/// multiplicities at each survivor's first left appearance).
fn combine_rows(op: SetOp, all: bool, l: Vec<Vec<Value>>, r: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    match (op, all) {
        (SetOp::Union, true) => {
            let mut out = l;
            out.extend(r);
            out
        }
        (SetOp::Union, false) => {
            let mut out = l;
            out.extend(r);
            sorted_unique(out)
        }
        (SetOp::Intersect, false) => {
            let right = sorted_unique(r);
            sorted_unique(l)
                .into_iter()
                .filter(|row| right.binary_search_by(|probe| row_cmp(probe, row)).is_ok())
                .collect()
        }
        (SetOp::Except, false) => {
            let right = sorted_unique(r);
            sorted_unique(l)
                .into_iter()
                .filter(|row| right.binary_search_by(|probe| row_cmp(probe, row)).is_err())
                .collect()
        }
        (SetOp::Intersect, true) => multiset(&l, &r, |cl, cr| cl.min(cr)),
        (SetOp::Except, true) => multiset(&l, &r, |cl, cr| cl.saturating_sub(cr)),
    }
}

/// Multiset arithmetic: each distinct left row is emitted
/// `f(count_left, count_right)` times (min for INTERSECT ALL,
/// subtraction for EXCEPT ALL), at its first left appearance. NULLs
/// equal, like the dedup path.
fn multiset(
    l: &[Vec<Value>],
    r: &[Vec<Value>],
    f: impl Fn(usize, usize) -> usize,
) -> Vec<Vec<Value>> {
    // Sorted copy of the right side: occurrence counts resolve by
    // partition points around the equal-run.
    let mut right = r.to_vec();
    right.sort_by(|a, b| row_cmp(a, b));
    let count_right = |target: &[Value]| {
        let lo = right.partition_point(|x| row_cmp(x, target) == std::cmp::Ordering::Less);
        let hi = right.partition_point(|x| row_cmp(x, target) != std::cmp::Ordering::Greater);
        hi - lo
    };
    // Run-length encoding of the left side, first-appearance order.
    let mut counts: Vec<(Vec<Value>, usize)> = Vec::new();
    for row in l {
        match counts
            .iter_mut()
            .find(|(x, _)| row_cmp(x, row) == std::cmp::Ordering::Equal)
        {
            Some((_, n)) => *n += 1,
            None => counts.push((row.clone(), 1)),
        }
    }
    let mut out = Vec::new();
    for (row, cl) in counts {
        for _ in 0..f(cl, count_right(&row)) {
            out.push(row.clone());
        }
    }
    out
}

/// Sort and dedup whole rows (row_cmp order, NULLs first and equal).
fn sorted_unique(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|a, b| row_cmp(a, b));
    rows.dedup_by(|a, b| row_cmp(a, b) == std::cmp::Ordering::Equal);
    rows
}

/// Columnwise SQL comparison; inhomogeneous cells compare Equal so
/// dedup treats only type-compatible rows as duplicates.
fn row_cmp(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    for (x, y) in a.iter().zip(b) {
        let ord = match (x, y) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Less,
            (_, Value::Null) => std::cmp::Ordering::Greater,
            _ => cmp_values(x, y).unwrap_or(std::cmp::Ordering::Equal),
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

fn widen(a: SqlType, b: SqlType) -> SqlResult<SqlType> {
    use SqlType::*;
    match (a, b) {
        (x, y) if x == y => Ok(x),
        (Int, Double) | (Double, Int) => Ok(Double),
        // Decimal families widen to the coarser scale (Ints under a
        // decimal column lift to scale 0) and to Double against it.
        (Decimal { scale: sa, .. }, Decimal { scale: sb, .. }) => Ok(Decimal {
            precision: crate::sql::storage::schema::MAX_DECIMAL_SCALE,
            scale: sa.max(sb),
        }),
        (Decimal { scale, .. }, Int) | (Int, Decimal { scale, .. }) => Ok(Decimal {
            precision: crate::sql::storage::schema::MAX_DECIMAL_SCALE,
            scale,
        }),
        (Decimal { .. }, Double) | (Double, Decimal { .. }) => Ok(Double),
        // A date column under a datetime column widens to datetime, so
        // dedup ordering and rendering use full precision.
        (Date, DateTime) | (DateTime, Date) => Ok(DateTime),
        (a, b) => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("set operation of incompatible column types {a:?} and {b:?}"),
        )),
    }
}

/// Normalize cells widened by [`widen`]: Date cells of a column typed
/// DateTime lift to midnight microseconds, and widened decimal columns
/// carry every cell at the column scale (Int lifts exactly, Decimal
/// rescales half-away-from-zero, Decimal under Double coarsens) -- so
/// dedup and rendering see one spelling per value.
fn widen_cells(columns: &[crate::sql::exec::ColMeta], rows: &mut [Vec<Value>]) -> SqlResult<()> {
    use crate::sql::exec::expr_decimal::{decimal_to_f64, pow10, rescale_decimal};
    for row in rows {
        for (col, cell) in columns.iter().zip(row.iter_mut()) {
            match (&*cell, col.sql_type) {
                (Value::Date(d), SqlType::DateTime) => {
                    *cell = Value::DateTime(
                        d.checked_mul(crate::sql::temporal::MICROS_PER_DAY)
                            .ok_or_else(|| {
                                SqlError::new(
                                    ErrorCode::NotSupported,
                                    format!("date {d} out of DATETIME range"),
                                )
                            })?,
                    );
                }
                (Value::Int(i), SqlType::Decimal { scale, .. }) => {
                    *cell = Value::Decimal(
                        i128::from(*i).checked_mul(pow10(scale)).ok_or_else(|| {
                            SqlError::new(
                                ErrorCode::NotSupported,
                                format!("integer {i} out of DECIMAL({scale}) range"),
                            )
                        })?,
                        scale,
                    );
                }
                (Value::Decimal(m, s), SqlType::Decimal { scale, .. }) => {
                    *cell = Value::Decimal(rescale_decimal(*m, *s, scale)?, scale);
                }
                (Value::Decimal(m, s), SqlType::Double) => {
                    *cell = Value::Double(decimal_to_f64(*m, *s));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Trailing ORDER BY / LIMIT of the compound: keys resolve against
/// the output columns (ordinal `ORDER BY 2` selects column 2, MySQL
/// style for set operations).
fn apply_tail(
    rel: &mut Relation,
    keys: &[OrderKey],
    limit: Option<&LimitValue>,
    offset: &LimitValue,
) -> SqlResult<()> {
    if keys.is_empty() && limit.is_none() && *offset == LimitValue::zero() {
        return Ok(());
    }
    let mut rows = match Arc::get_mut(&mut rel.rows) {
        Some(v) => std::mem::take(v),
        None => rel.rows.as_slice().to_vec(),
    };
    if !keys.is_empty() {
        let scope = rel.scope("");
        let mut pairs: Vec<(Vec<Value>, Vec<Value>)> = rows
            .into_iter()
            .map(|row| {
                let key_vals = keys
                    .iter()
                    .map(|k| ordinal_or_eval(k, &scope, &row))
                    .collect::<SqlResult<Vec<_>>>()?;
                Ok((key_vals, row))
            })
            .collect::<SqlResult<Vec<_>>>()?;
        pairs.sort_by(|a, b| {
            for (i, k) in keys.iter().enumerate() {
                let ord = select::cmp_null_first(&a.0[i], &b.0[i]);
                let ord = if k.asc { ord } else { ord.reverse() };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        rows = pairs.into_iter().map(|(_, r)| r).collect();
    }
    // LIMIT / OFFSET coerce once: a bound `?` must be a non-negative
    // integer, exactly like a numeric literal.
    let take = limit.map(limit_u64).transpose()?.map(|l| l as usize);
    let take = take.unwrap_or(usize::MAX);
    let skip = limit_u64(offset)? as usize;
    rows = rows.into_iter().skip(skip).take(take).collect();
    rel.rows = Arc::new(rows);
    Ok(())
}

/// `ORDER BY <integer literal>` on a set operation means the output
/// column at that position (1-based); everything else evaluates
/// against the output columns.
fn ordinal_or_eval(
    k: &OrderKey,
    scope: &crate::sql::exec::scan::FromScope,
    row: &[Value],
) -> SqlResult<Value> {
    if let crate::sql::parse::ast::Expr::Lit(Value::Int(n)) = &k.expr {
        let idx = *n - 1;
        if idx < 0 || idx as usize >= scope.row_width() {
            return Err(SqlError::new(
                ErrorCode::BadField,
                format!("ORDER BY ordinal {n} is out of range"),
            ));
        }
        return Ok(row[idx as usize].clone());
    }
    eval(&k.expr, scope, row)
}

#[cfg(test)]
#[path = "set_ops_tests.rs"]
mod set_ops_tests;
