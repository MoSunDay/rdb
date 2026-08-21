//! SELECT pipeline: scan -> join -> filter -> group/aggregate -> having
//! -> project -> distinct -> order -> limit/offset.
//!
//! The algebra lives in pure functions over a [`scan::Source`]; `run`
//! only supplies the snapshot timestamp and the catalog-backed source,
//! so everything below `execute_query` is unit-testable without any
//! storage. Aggregate evaluation lives in `agg.rs`, EXPLAIN rendering
//! in `render.rs`.

use crate::sql::dist::gather;
use crate::sql::exec::agg::{eval_in_group, has_agg, Unit};
use crate::sql::exec::expr::{cmp_values, eval, truthy};
use crate::sql::exec::render;
use crate::sql::exec::scan::{self, FromScope, Source};
use crate::sql::exec::{ColMeta, ExecOutcome, SqlSession};
use crate::sql::parse::ast::{
    AggFunc, BinOp, Expr, OrderKey, Query, SelectItem, Statement, TableRef,
};
use crate::sql::parse::error::SqlResult;
use crate::sql::plan;
use crate::sql::storage::schema::{SqlType, Value};
use crate::state::Shared;

/// Execute a SELECT as a snapshot read. Autocommit reads run at the
/// oracle's `now()`; inside a transaction the read is pinned to the
/// txn's `read_ts` and merged with its staged writes.
pub async fn run(
    shared: &Shared,
    sess: &SqlSession,
    q: Query,
) -> SqlResult<(Vec<ColMeta>, Vec<Vec<Value>>)> {
    // FOR UPDATE degrades to a plain snapshot read in M1; the explicit
    // txn's write-write validation at COMMIT supplies the serialization.
    let (read_ts, txn) = match sess.txn.as_ref() {
        Some(t) => (t.read_ts, Some(t)),
        None => (shared.sql_ts.now(), None),
    };
    // Multi-node clusters read scatter-gather (falls back to the local
    // scan path for joins / single-node topologies).
    let src = gather::materialize(shared, &q.from, read_ts, txn, q.filter.as_ref()).await?;
    execute_query(&q, &src)
}

/// EXPLAIN: render a plan rowset (see `render.rs`). The headline
/// carries the scatter-gather banner plus a SeqScan line in cluster
/// mode (indexes only cover the owning band, so the planner is not
/// consulted), else the planner's IndexScan verdict; everything below
/// the headline is pure IR.
pub fn explain(shared: &Shared, stmt: &Statement) -> SqlResult<ExecOutcome> {
    let headline = match stmt {
        Statement::Select(q) => headline_lines(shared, q),
        _ => Vec::new(),
    };
    render::explain(stmt, headline)
}

/// EXPLAIN headline lines for a SELECT. Gatherable FROMs read
/// scatter-gather: the banner rides ABOVE the plain SeqScan line
/// (bands are scanned, per node, exactly as a local SeqScan would).
fn headline_lines(shared: &Shared, q: &Query) -> Vec<String> {
    if let Some(banner) = gather::headline(shared, &q.from) {
        return vec![banner, format!("SeqScan {}", render::from_display(&q.from))];
    }
    access_line(shared, q).into_iter().collect()
}

/// Planner verdict for the EXPLAIN headline: `IndexScan <idx> -> N pks`
/// when the query would read through an index, else the plain SeqScan
/// line. Planning here is exactly what `scan::materialize` will do, so
/// the explained plan and the executed plan cannot drift.
fn access_line(shared: &Shared, q: &Query) -> Option<String> {
    let TableRef::Table { name, alias } = &q.from else {
        return None; // joins always plan as SeqScan
    };
    let schema = crate::sql::storage::catalog::lookup(shared, name)
        .ok()
        .flatten()?;
    match plan::plan(&shared.store, &schema, alias.as_deref(), q.filter.as_ref()) {
        plan::Path::IndexLookup { index, pks, .. } => {
            Some(format!("IndexScan {} -> {} pks", index.name, pks.len()))
        }
        plan::Path::SeqScan => None,
    }
}

/// The whole query over a materialized source. Pure.
pub fn execute_query(q: &Query, src: &Source) -> SqlResult<(Vec<ColMeta>, Vec<Vec<Value>>)> {
    let scope = &src.scope;
    validate_refs(q, scope)?;

    let items = expand_items(&q.items, scope)?;
    let rows = filter_rows(&src.rows, scope, q.filter.as_ref())?;

    let grouped = !q.group_by.is_empty()
        || items.iter().any(|(e, _)| has_agg(e))
        || q.having.as_ref().is_some_and(has_agg)
        || q.order_by.iter().any(|k| has_agg(&k.expr));

    let mut units: Vec<Unit> = if grouped {
        let mut groups = group_by_keys(rows, scope, &q.group_by)?;
        if q.group_by.is_empty() && groups.is_empty() {
            // Aggregates without GROUP BY run as ONE global group, even
            // over zero rows: COUNT(*) = 0, SUM/MIN/... = NULL.
            groups.push(Vec::new());
        }
        let null_row = vec![Value::Null; scope.row_width()];
        groups
            .into_iter()
            .map(|g| Unit {
                rep: g.first().cloned().unwrap_or_else(|| null_row.clone()),
                rows: g,
            })
            .collect()
    } else {
        rows.into_iter()
            .map(|r| Unit {
                rep: r.clone(),
                rows: vec![r],
            })
            .collect()
    };

    if let Some(h) = &q.having {
        let mut kept = Vec::with_capacity(units.len());
        for u in units {
            if truthy(&eval_in_group(h, scope, &u)?)? {
                kept.push(u);
            }
        }
        units = kept;
    }

    // (order values, projected row) pairs: order keys are evaluated in
    // the same row/group context as the projection.
    let mut pairs: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(units.len());
    for u in &units {
        let mut projected = Vec::with_capacity(items.len());
        for (e, _) in &items {
            projected.push(eval_in_group(e, scope, u)?);
        }
        let mut order_vals = Vec::with_capacity(q.order_by.len());
        for k in &q.order_by {
            order_vals.push(eval_in_group(&k.expr, scope, u)?);
        }
        pairs.push((order_vals, projected));
    }

    if q.distinct {
        dedupe_pairs(&mut pairs);
    }
    sort_pairs(&mut pairs, &q.order_by);

    let take = q.limit.map(|l| l as usize).unwrap_or(usize::MAX);
    let rows = pairs
        .into_iter()
        .skip(q.offset as usize)
        .take(take)
        .map(|(_, r)| r)
        .collect();
    let columns = items.into_iter().map(|(_, m)| m).collect();
    Ok((columns, rows))
}

/// Fail fast on unknown/ambiguous column references before evaluation.
fn validate_refs(q: &Query, scope: &FromScope) -> SqlResult<()> {
    if let Some(f) = &q.filter {
        scan::check_expr(f, scope)?;
    }
    for g in &q.group_by {
        scan::check_expr(g, scope)?;
    }
    if let Some(h) = &q.having {
        scan::check_expr(h, scope)?;
    }
    for k in &q.order_by {
        scan::check_expr(&k.expr, scope)?;
    }
    for item in &q.items {
        if let SelectItem::Expr { expr, .. } = item {
            scan::check_expr(expr, scope)?;
        }
    }
    Ok(())
}

/// Keep rows whose WHERE evaluates true (NULL/Unknown excludes).
pub fn filter_rows(
    rows: &[Vec<Value>],
    scope: &FromScope,
    filter: Option<&Expr>,
) -> SqlResult<Vec<Vec<Value>>> {
    let Some(f) = filter else {
        return Ok(rows.to_vec());
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if truthy(&eval(f, scope, r)?)? {
            out.push(r.clone());
        }
    }
    Ok(out)
}

/// Group rows by evaluated group-by key values (NULLs group together
/// via Value equality). Group order = first appearance.
fn group_by_keys(
    rows: Vec<Vec<Value>>,
    scope: &FromScope,
    keys: &[Expr],
) -> SqlResult<Vec<Vec<Vec<Value>>>> {
    let mut groups: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
    for row in rows {
        let mut key = Vec::with_capacity(keys.len());
        for k in keys {
            key.push(eval(k, scope, &row)?);
        }
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(row),
            None => groups.push((key, vec![row])),
        }
    }
    Ok(groups.into_iter().map(|(_, g)| g).collect())
}

/// Expand the select list into (expr, metadata) pairs; `*` becomes
/// every column of the FROM scope, in side order.
fn expand_items(items: &[SelectItem], scope: &FromScope) -> SqlResult<Vec<(Expr, ColMeta)>> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for side in &scope.sides {
                    for (i, col) in side.columns.iter().enumerate() {
                        out.push((
                            Expr::Col {
                                table: Some(side.qualifier.clone()),
                                name: col.clone(),
                            },
                            ColMeta {
                                table: side.qualifier.clone(),
                                name: col.clone(),
                                sql_type: side.types[i],
                            },
                        ));
                    }
                }
            }
            SelectItem::Expr { expr, alias } => {
                let (table, name) = output_name(expr, alias, scope);
                out.push((
                    expr.clone(),
                    ColMeta {
                        table,
                        name,
                        sql_type: result_type(expr, scope),
                    },
                ));
            }
        }
    }
    Ok(out)
}

/// Output column naming: alias, else the column name (plain Col), else
/// the rendered expression. Table qualifier: the Col's own qualifier,
/// else the owning side's, else "" for computed columns.
fn output_name(expr: &Expr, alias: &Option<String>, scope: &FromScope) -> (String, String) {
    match expr {
        Expr::Col { table, name } => {
            let t = match table {
                Some(t) => t.clone(),
                None => scope
                    .sides
                    .iter()
                    .find(|s| s.columns.iter().any(|c| c.eq_ignore_ascii_case(name)))
                    .map(|s| s.qualifier.clone())
                    .unwrap_or_default(),
            };
            (t, alias.clone().unwrap_or_else(|| name.clone()))
        }
        other => (
            String::new(),
            alias.clone().unwrap_or_else(|| render::expr_display(other)),
        ),
    }
}

/// Best-effort static result type of an output expression.
fn result_type(e: &Expr, scope: &FromScope) -> SqlType {
    match e {
        Expr::Lit(v) => v.sql_type().unwrap_or(SqlType::VarChar),
        Expr::Col { table, name } => scope
            .resolve_checked(table.as_deref(), name)
            .ok()
            .and_then(|idx| scope.type_at(idx))
            .unwrap_or(SqlType::VarChar),
        Expr::Agg { func, arg, .. } => match func {
            AggFunc::Count => SqlType::Int,
            // SUM keeps its integer width; AVG always yields a double.
            AggFunc::Sum => match arg.as_deref() {
                Some(a) if result_type(a, scope) == SqlType::Int => SqlType::Int,
                _ => SqlType::Double,
            },
            AggFunc::Avg => SqlType::Double,
            AggFunc::Min | AggFunc::Max => arg
                .as_deref()
                .map(|a| result_type(a, scope))
                .unwrap_or(SqlType::VarChar),
        },
        Expr::Neg(x) => result_type(x, scope),
        Expr::Not(_)
        | Expr::IsNull { .. }
        | Expr::InList { .. }
        | Expr::Between { .. }
        | Expr::Like { .. } => SqlType::Bool,
        Expr::BinaryOp { left, op, right } => match op {
            BinOp::And
            | BinOp::Or
            | BinOp::Eq
            | BinOp::NotEq
            | BinOp::Lt
            | BinOp::LtEq
            | BinOp::Gt
            | BinOp::GtEq => SqlType::Bool,
            _ => {
                if result_type(left, scope) == SqlType::Double
                    || result_type(right, scope) == SqlType::Double
                {
                    SqlType::Double
                } else {
                    SqlType::Int
                }
            }
        },
        Expr::Func { .. } | Expr::Placeholder => SqlType::VarChar,
    }
}

/// Stable sort by pre-evaluated order keys; NULLs sort smallest (first
/// ascending, last descending).
pub fn sort_pairs(pairs: &mut [(Vec<Value>, Vec<Value>)], keys: &[OrderKey]) {
    pairs.sort_by(|a, b| {
        for (i, k) in keys.iter().enumerate() {
            let ord = if k.asc {
                cmp_null_first(&a.0[i], &b.0[i])
            } else {
                cmp_null_first(&a.0[i], &b.0[i]).reverse()
            };
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
        }
        std::cmp::Ordering::Equal
    });
}

/// SQL ordering with NULLs smallest; inhomogeneous pairs compare Equal.
pub fn cmp_null_first(l: &Value, r: &Value) -> std::cmp::Ordering {
    match (l, r) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        (a, b) => cmp_values(a, b).unwrap_or(std::cmp::Ordering::Equal),
    }
}

/// DISTINCT over projected rows (first occurrence wins).
fn dedupe_pairs(pairs: &mut Vec<(Vec<Value>, Vec<Value>)>) {
    let mut seen: Vec<Vec<Value>> = Vec::with_capacity(pairs.len());
    pairs.retain(|(_, projected)| {
        if seen.contains(projected) {
            false
        } else {
            seen.push(projected.clone());
            true
        }
    });
}

/// ORDER BY a row set (UPDATE/DELETE reuse this): evaluate the keys per
/// row, stable-sort, write the rows back in order.
pub fn order_rows(
    rows: &mut Vec<Vec<Value>>,
    keys: &[OrderKey],
    scope: &FromScope,
) -> SqlResult<()> {
    let mut pairs: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for r in rows.drain(..) {
        let mut kv = Vec::with_capacity(keys.len());
        for k in keys {
            kv.push(eval(&k.expr, scope, &r)?);
        }
        pairs.push((kv, r));
    }
    sort_pairs(&mut pairs, keys);
    *rows = pairs.into_iter().map(|(_, r)| r).collect();
    Ok(())
}

#[cfg(test)]
#[path = "select_tests.rs"]
mod tests;
