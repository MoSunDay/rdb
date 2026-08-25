//! Compound query execution: CTEs (WITH), UNION [ALL], and the
//! trailing ORDER BY / LIMIT that belong to the whole compound.
//!
//! Each operand materializes into a [`Relation`] (SELECTs run through
//! the normal select pipeline -- including cluster scatter-gather --
//! at the caller's snapshot timestamp, so a compound never mixes read
//! points). UNION validates column arity, widens numeric columns,
//! dedups plain UNION, and adopts the left operand's column names,
//! like MySQL.

use std::sync::Arc;

use crate::sql::exec::expr::{cmp_values, eval};
use crate::sql::exec::relation::{relation_of, CteScope, Relation};
use crate::sql::exec::select;
use crate::sql::exec::subquery::SubqCtx;
use crate::sql::parse::ast::{CompoundQuery, OrderKey, QueryBody};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
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
    };
    let mut rel = Box::pin(eval_body(shared, read_ts, txn, &cq.body, &ctx)).await?;
    apply_tail(&mut rel, &cq.order_by, cq.limit, cq.offset)?;
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
        QueryBody::Union { left, right, all } => {
            let l = Box::pin(eval_body(shared, read_ts, txn, left, ctx)).await?;
            let r = Box::pin(eval_body(shared, read_ts, txn, right, ctx)).await?;
            union_relations(l, r, *all)
        }
    }
}

/// UNION semantics: arity must match, numeric columns widen
/// (INT|DOUBLE -> DOUBLE), plain UNION dedups (NULLs equal, like
/// MySQL), UNION ALL concatenates. Column names come from the left.
fn union_relations(l: Relation, r: Relation, all: bool) -> SqlResult<Relation> {
    if l.columns.len() != r.columns.len() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "UNION operands yield different column counts ({} vs {})",
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
            Ok(crate::sql::exec::ColMeta {
                table: a.table.clone(),
                name: a.name.clone(),
                sql_type,
            })
        })
        .collect::<SqlResult<Vec<_>>>()?;
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(l.rows.len() + r.rows.len());
    rows.extend(l.rows.iter().cloned());
    rows.extend(r.rows.iter().cloned());
    widen_cells(&columns, &mut rows)?;
    if !all {
        rows.sort_by(|a, b| row_cmp(a, b));
        rows.dedup_by(|a, b| row_cmp(a, b) == std::cmp::Ordering::Equal);
    }
    Ok(relation_of(columns, rows))
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
        // A date column under a datetime column widens to datetime, so
        // dedup ordering and rendering use full precision.
        (Date, DateTime) | (DateTime, Date) => Ok(DateTime),
        (a, b) => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("UNION of incompatible column types {a:?} and {b:?}"),
        )),
    }
}

/// Normalize cells widened by [`widen`]: Date cells of a column typed
/// DateTime lift to midnight microseconds. (Int cells of a widened
/// Double column flow through untouched, as they always have.)
fn widen_cells(columns: &[crate::sql::exec::ColMeta], rows: &mut [Vec<Value>]) -> SqlResult<()> {
    for row in rows {
        for (col, cell) in columns.iter().zip(row.iter_mut()) {
            if let (Value::Date(d), SqlType::DateTime) = (&*cell, col.sql_type) {
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
    limit: Option<u64>,
    offset: u64,
) -> SqlResult<()> {
    if keys.is_empty() && limit.is_none() && offset == 0 {
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
    let take = limit.map(|l| l as usize).unwrap_or(usize::MAX);
    rows = rows.into_iter().skip(offset as usize).take(take).collect();
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
mod tests {
    use super::*;
    use crate::sql::exec::ColMeta;
    use crate::sql::storage::schema::SqlType;

    fn col(name: &str, ty: SqlType) -> ColMeta {
        ColMeta {
            table: String::new(),
            name: name.to_string(),
            sql_type: ty,
        }
    }

    fn rel(cols: Vec<ColMeta>, rows: Vec<Vec<Value>>) -> Relation {
        Relation::new(cols, rows)
    }

    #[test]
    fn union_all_concatenates_and_keeps_duplicates() {
        let l = rel(
            vec![col("a", SqlType::Int)],
            vec![vec![Value::Int(1)], vec![Value::Int(1)]],
        );
        let r = rel(vec![col("b", SqlType::Int)], vec![vec![Value::Int(1)]]);
        let out = union_relations(l, r, true).unwrap();
        assert_eq!(out.rows.len(), 3);
        // left operand names the output column.
        assert_eq!(out.columns[0].name, "a");
    }

    #[test]
    fn union_distinct_dedups_rows_and_nulls() {
        let l = rel(
            vec![col("a", SqlType::Int)],
            vec![vec![Value::Null], vec![Value::Int(1)], vec![Value::Null]],
        );
        let r = rel(
            vec![col("b", SqlType::Int)],
            vec![vec![Value::Int(1)], vec![Value::Int(2)]],
        );
        let out = union_relations(l, r, false).unwrap();
        let vals: Vec<&Value> = out.rows.iter().map(|r| &r[0]).collect();
        assert_eq!(vals, vec![&Value::Null, &Value::Int(1), &Value::Int(2)]);
    }

    #[test]
    fn union_arity_mismatch_and_type_widen() {
        let l1 = rel(vec![col("a", SqlType::Int)], vec![vec![Value::Int(1)]]);
        let r2 = rel(
            vec![col("x", SqlType::Int), col("y", SqlType::Int)],
            vec![vec![Value::Int(1), Value::Int(2)]],
        );
        assert!(union_relations(l1.clone(), r2, true).is_err());

        let rd = rel(
            vec![col("d", SqlType::Double)],
            vec![vec![Value::Double(1.5)]],
        );
        let out = union_relations(l1, rd, true).unwrap();
        assert_eq!(out.columns[0].sql_type, SqlType::Double);

        let rs = rel(
            vec![col("s", SqlType::VarChar)],
            vec![vec![Value::Str("x".into())]],
        );
        let l1 = rel(vec![col("a", SqlType::Int)], vec![vec![Value::Int(1)]]);
        assert!(union_relations(l1, rs, true).is_err());
    }

    #[test]
    fn union_widens_date_to_datetime_and_lifts_cells() {
        use crate::sql::temporal::MICROS_PER_DAY;
        let day = rel(
            vec![col("d", SqlType::Date)],
            vec![vec![Value::Date(19_782)]],
        );
        let stamp = rel(
            vec![col("t", SqlType::DateTime)],
            vec![vec![Value::DateTime(19_783 * MICROS_PER_DAY)]],
        );
        // date | datetime -> datetime, and the Date cell lifts to
        // midnight microseconds so rendering keeps full precision.
        let out = union_relations(day.clone(), stamp.clone(), true).unwrap();
        assert_eq!(out.columns[0].sql_type, SqlType::DateTime);
        assert_eq!(
            out.rows.as_slice(),
            &[
                vec![Value::DateTime(19_782 * MICROS_PER_DAY)],
                vec![Value::DateTime(19_783 * MICROS_PER_DAY)]
            ]
        );
        // operand order does not matter
        let out = union_relations(stamp, day, true).unwrap();
        assert_eq!(out.columns[0].sql_type, SqlType::DateTime);
        // plain UNION dedups the widened midnight pair
        let l = rel(
            vec![col("d", SqlType::Date)],
            vec![vec![Value::Date(19_782)]],
        );
        let r = rel(
            vec![col("t", SqlType::DateTime)],
            vec![vec![Value::DateTime(19_782 * MICROS_PER_DAY)]],
        );
        let out = union_relations(l, r, false).unwrap();
        assert_eq!(
            out.rows.as_slice(),
            &[vec![Value::DateTime(19_782 * MICROS_PER_DAY)]]
        );
        // temporal never mixes with numerics or text
        let nums = rel(vec![col("n", SqlType::Int)], vec![vec![Value::Int(1)]]);
        let l = rel(vec![col("d", SqlType::Date)], vec![vec![Value::Date(0)]]);
        assert!(union_relations(l, nums, true).is_err());
    }
}
