//! Correlated-subquery binding: per-outer-row evaluation without
//! touching the (pure, synchronous) expression evaluator.
//!
//! The pre-materialization pass (`exec::subquery`) folds uncorrelated
//! subqueries into constants; a subquery whose body references outer
//! columns cannot fold and stays in the tree as a deferred node. This
//! module runs AFTER the outer FROM materialized: for every deferred
//! node it finds the outer references (names that resolve against the
//! outer row scope but not against the subquery's own FROM), then
//! evaluates the subquery once per DISTINCT outer binding -- outer
//! columns substituted as literals -- and replaces the node with a
//! pure `Expr::Correlated` lookup. Nested (child-to-parent) levels
//! chain naturally: each level binds against its own outer rows.
//! N+1 evaluation per distinct binding is accepted for v1; no
//! decorrelation is attempted.
//!
//! Narrow shapes that stay unsupported (each fails loudly, never
//! silently): outer references inside a derived table body or a CTE
//! body of the subquery (the shadow set below knows their OUTPUT
//! names, but the rewrite never visits inside their bodies), skip-
//! level references (grandchild straight to grandparent scope), and
//! correlated JOIN conditions.

use std::borrow::Cow;

use crate::sql::exec::correlated_map::map_compound;
use crate::sql::exec::expr::eval;
use crate::sql::exec::scan::{FromScope, Source};
use crate::sql::exec::set_ops::run_compound;
use crate::sql::exec::subquery::{materialize_uncorrelated, scalar_of, SubqCtx};
use crate::sql::parse::ast::{
    CompoundQuery, CorrelatedKind, CorrelatedOut, Cte, Expr, Query, QueryBody, SelectItem, TableRef,
};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;

/// Second (bind) pass over a rewritten query: every deferred subquery
/// node becomes a bound `Expr::Correlated`. Queries without deferred
/// nodes return borrowed (zero cost on the common path).
pub async fn bind_query<'a>(
    q: &'a Query,
    src: &Source,
    ctx: &SubqCtx<'_>,
) -> SqlResult<Cow<'a, Query>> {
    if !has_subquery_node(q) {
        return Ok(Cow::Borrowed(q));
    }
    let bind_ctx = SubqCtx {
        shared: ctx.shared,
        read_ts: ctx.read_ts,
        txn: ctx.txn,
        ctes: ctx.ctes,
        outer: Some(crate::sql::exec::subquery::OuterRows {
            scope: &src.scope,
            rows: &src.rows,
        }),
    };
    Ok(Cow::Owned(
        crate::sql::exec::subquery::rewrite_query(q, &bind_ctx).await?,
    ))
}

/// Whether any expression position of the query still carries a raw
/// (deferred) subquery node: the cheap gate for the bind pass.
pub fn has_subquery_node(q: &Query) -> bool {
    q.items.iter().any(|i| match i {
        SelectItem::Expr { expr, .. } => expr_has_subquery(expr),
        SelectItem::Wildcard => false,
    }) || q.filter.as_ref().is_some_and(expr_has_subquery)
        || q.group_by.iter().any(expr_has_subquery)
        || q.having.as_ref().is_some_and(expr_has_subquery)
        || q.order_by.iter().any(|k| expr_has_subquery(&k.expr))
}

pub(crate) fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => true,
        Expr::BinaryOp { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Not(x) | Expr::Neg(x) => expr_has_subquery(x),
        Expr::IsNull { expr, .. } => expr_has_subquery(expr),
        Expr::InList { expr, list, .. } => {
            expr_has_subquery(expr) || list.iter().any(expr_has_subquery)
        }
        Expr::Between {
            expr, low, high, ..
        } => expr_has_subquery(expr) || expr_has_subquery(low) || expr_has_subquery(high),
        Expr::Like { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            operand.as_deref().is_some_and(expr_has_subquery)
                || branches
                    .iter()
                    .any(|(c, t)| expr_has_subquery(c) || expr_has_subquery(t))
                || else_expr.as_deref().is_some_and(expr_has_subquery)
        }
        Expr::Cast { expr, .. } => expr_has_subquery(expr),
        Expr::Regexp { expr, pattern, .. } => expr_has_subquery(expr) || expr_has_subquery(pattern),
        Expr::Agg { arg: Some(a), .. } => expr_has_subquery(a),
        Expr::Func { args, .. } => args.iter().any(expr_has_subquery),
        _ => false,
    }
}

/// Bind one deferred node against the outer rows: pre-compute the
/// subquery result per distinct outer binding. A node with no outer
/// reference here was deferred for another reason (typo, unsupported
/// position) -- re-running it uncorrelated surfaces that error
/// verbatim.
pub(crate) async fn bind_node(
    e: &Expr,
    outer: crate::sql::exec::subquery::OuterRows<'_>,
    ctx: &SubqCtx<'_>,
) -> SqlResult<Expr> {
    let cq: &CompoundQuery = match e {
        Expr::Subquery(cq) => cq,
        Expr::InSubquery { query, .. } => query,
        Expr::Exists { query, .. } => query,
        _ => return materialize_uncorrelated(e, ctx).await,
    };
    let shadow = shadow_sides(ctx, cq);
    let cands = outer_refs(cq, outer.scope, &shadow, ctx);
    if cands.is_empty() {
        return materialize_uncorrelated(e, ctx).await;
    }
    let kind = match e {
        Expr::Subquery(_) => CorrelatedKind::Scalar,
        Expr::Exists { negated, .. } => CorrelatedKind::Exists { negated: *negated },
        Expr::InSubquery { expr, negated, .. } => CorrelatedKind::In {
            lhs: Box::new(crate::sql::exec::subquery::rewrite_expr(expr, ctx).await?),
            negated: *negated,
        },
        _ => return materialize_uncorrelated(e, ctx).await,
    };
    let mut cases: Vec<(Vec<Value>, CorrelatedOut)> = Vec::new();
    for row in outer.rows {
        let key: Vec<Value> = cands
            .iter()
            .map(|(ex, _)| eval(ex, outer.scope, row))
            .collect::<SqlResult<Vec<_>>>()?;
        if cases.iter().any(|(k, _)| *k == key) {
            continue; // memoize per distinct binding
        }
        let bound = substitute_refs(cq, outer.scope, &shadow, ctx, &cands, &key);
        let rel = run_compound(ctx.shared, ctx.read_ts, ctx.txn, &bound, ctx.ctes).await?;
        cases.push((key, out_of(e, &rel)?));
    }
    Ok(Expr::Correlated {
        kind,
        keys: cands.iter().map(|(ex, _)| ex.clone()).collect(),
        cases,
    })
}

/// One binding's subquery output, by subquery flavor.
fn out_of(e: &Expr, rel: &crate::sql::exec::relation::Relation) -> SqlResult<CorrelatedOut> {
    match e {
        // scalar: one column, empty -> NULL, >1 rows -> error 1242
        Expr::Subquery(_) => Ok(CorrelatedOut::Scalar(scalar_of(rel)?)),
        // EXISTS: limit-1 semantics, any row suffices
        Expr::Exists { .. } => Ok(CorrelatedOut::Rows(if rel.rows.is_empty() {
            Vec::new()
        } else {
            vec![Value::Int(1)]
        })),
        Expr::InSubquery { .. } => {
            if rel.columns.len() != 1 {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!(
                        "IN subquery must yield exactly one column (got {})",
                        rel.columns.len()
                    ),
                ));
            }
            Ok(CorrelatedOut::Rows(
                rel.rows.iter().map(|r| r[0].clone()).collect(),
            ))
        }
        _ => Err(SqlError::new(
            ErrorCode::NotSupported,
            "not a subquery node".to_string(),
        )),
    }
}

/// One statically-known side of the subquery's own FROM scope.
struct ShadowSide {
    qualifier: String,
    /// `None` when the side's columns are not statically known (table
    /// missing from the catalog, underivable projection): then a
    /// QUALIFIED reference through the qualifier still shadows,
    /// unqualified names cannot be attributed here.
    columns: Option<Vec<String>>,
}

/// Statically-known sides of the subquery's own scope: every SELECT
/// arm's FROM (join trees flattened) plus the output columns of its
/// own `WITH` entries. Base-table columns come from the catalog
/// (exactly what `scan::materialize` resolves against); a FROM name
/// that is not a catalog table but a visible CTE takes the
/// materialized relation's exact columns; derived tables and CTE
/// bodies contribute their statically-derivable output names
/// (`output_columns`). Every rule may only err towards MORE
/// shadowing: a false positive surfaces as the inner run's
/// unknown-column error, a false negative silently binds outer data.
fn shadow_sides(ctx: &SubqCtx<'_>, cq: &CompoundQuery) -> Vec<ShadowSide> {
    let mut out = Vec::new();
    compound_sides(ctx, cq, &mut out);
    out
}

/// Sides of one compound query: its `WITH` entries (qualifier = CTE
/// name) then its body's sides.
fn compound_sides(ctx: &SubqCtx<'_>, cq: &CompoundQuery, out: &mut Vec<ShadowSide>) {
    for cte in &cq.ctes {
        out.push(ShadowSide {
            qualifier: cte.name.clone(),
            columns: cte_output_columns(cte, ctx),
        });
    }
    body_sides(ctx, &cq.body, out);
}

fn body_sides(ctx: &SubqCtx<'_>, b: &QueryBody, out: &mut Vec<ShadowSide>) {
    match b {
        QueryBody::Select(q) => from_sides(ctx, &q.from, out),
        QueryBody::Nested(inner) => compound_sides(ctx, inner, out),
        // Both arms are in scope: each arm's own WHERE / projection
        // resolves against its own FROM sides.
        QueryBody::SetOp { left, right, .. } => {
            body_sides(ctx, left, out);
            body_sides(ctx, right, out);
        }
    }
}

fn from_sides(ctx: &SubqCtx<'_>, t: &TableRef, out: &mut Vec<ShadowSide>) {
    match t {
        TableRef::Table { name, alias } => {
            let columns = catalog_columns(ctx, name).or_else(|| visible_cte_columns(ctx, name));
            out.push(ShadowSide {
                qualifier: alias.clone().unwrap_or_else(|| name.clone()),
                columns,
            });
        }
        TableRef::NoTable => {}
        TableRef::Derived { query, alias } => out.push(ShadowSide {
            qualifier: alias.clone(),
            columns: output_columns(ctx, &query.body),
        }),
        TableRef::Join { left, right, .. } => {
            from_sides(ctx, left, out);
            from_sides(ctx, right, out);
        }
    }
}

/// A base table's column names, straight from the catalog.
fn catalog_columns(ctx: &SubqCtx<'_>, name: &str) -> Option<Vec<String>> {
    crate::sql::storage::catalog::lookup(ctx.shared, name)
        .ok()
        .flatten()
        .map(|s| s.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
}

/// A FROM name that is no catalog table: a CTE visible here (an
/// outer `WITH`, or this query's own `WITH` once the executor pushed
/// it) -- the already-materialized relation carries its exact output
/// names.
fn visible_cte_columns(ctx: &SubqCtx<'_>, name: &str) -> Option<Vec<String>> {
    ctx.ctes
        .lookup(name)
        .map(|rel| rel.columns.iter().map(|c| c.name.clone()).collect())
}

/// A CTE's output names: the positional alias list wins (the run
/// applies it verbatim), else the query's derivable projection names.
fn cte_output_columns(cte: &Cte, ctx: &SubqCtx<'_>) -> Option<Vec<String>> {
    if !cte.column_aliases.is_empty() {
        return Some(cte.column_aliases.clone());
    }
    output_columns(ctx, &cte.query.body)
}

/// Statically-derivable output names of a query body's top-level
/// projection: an item's alias, else a bare column's name; a wildcard
/// unions the inner FROM's side columns (a safe superset -- a join
/// tree exposes every side's columns positionally). Any item whose
/// name is not derivable poisons the whole list to `None` (the
/// qualifier still shadows qualified references). Set operations name
/// their columns after the LEFT arm, like MySQL.
fn output_columns(ctx: &SubqCtx<'_>, b: &QueryBody) -> Option<Vec<String>> {
    let select = match b {
        QueryBody::Select(q) => q,
        QueryBody::SetOp { left, .. } => return output_columns(ctx, left),
        QueryBody::Nested(inner) => return output_columns(ctx, &inner.body),
    };
    let mut names: Vec<String> = Vec::new();
    for item in &select.items {
        match item {
            SelectItem::Wildcard => {
                let mut inner = Vec::new();
                from_sides(ctx, &select.from, &mut inner);
                for side in inner {
                    names.extend(side.columns?);
                }
            }
            SelectItem::Expr { expr, alias } => match (alias, expr) {
                (Some(a), _) => names.push(a.clone()),
                (None, Expr::Col { name, .. }) => names.push(name.clone()),
                (None, _) => return None,
            },
        }
    }
    Some(names)
}

/// Whether a reference is the subquery's own (inner scope shadows
/// outer, per SQL): FROM sides by qualifier+column -- base tables
/// from the catalog, visible CTEs from their materialized relation,
/// derived tables and the subquery's own `WITH` from statically
/// derived output names -- and any CTE (outer `ctes` or the
/// subquery's own `WITH`) by qualifier alone. Shadowing errs wide:
/// a false positive fails the inner run loudly with unknown-column,
/// never silently binds outer data.
fn shadowed(
    table: Option<&str>,
    name: &str,
    sides: &[ShadowSide],
    ctx: &SubqCtx<'_>,
    cq: &CompoundQuery,
) -> bool {
    let Some(q) = table else {
        return sides.iter().any(|s| {
            s.columns
                .as_ref()
                .is_some_and(|c| c.iter().any(|x| x.eq_ignore_ascii_case(name)))
        });
    };
    ctx.ctes.lookup(q).is_some()
        || cq.ctes.iter().any(|c| c.name.eq_ignore_ascii_case(q))
        || sides.iter().any(|s| {
            s.qualifier.eq_ignore_ascii_case(q)
                && s.columns
                    .as_ref()
                    .is_none_or(|c| c.iter().any(|x| x.eq_ignore_ascii_case(name)))
        })
}

/// The subquery's outer references: column references (in its own
/// expression positions -- projections, WHERE / GROUP BY / HAVING /
/// ORDER BY and JOIN conditions, NOT nested subquery bodies) that
/// resolve against the OUTER scope but are not the subquery's own.
/// Returned as (first-seen spelling, outer row index), deduplicated
/// by index in first-appearance order -- that order fixes the key
/// tuples.
fn outer_refs(
    cq: &CompoundQuery,
    scope: &FromScope,
    sides: &[ShadowSide],
    ctx: &SubqCtx<'_>,
) -> Vec<(Expr, usize)> {
    let mut out: Vec<(Expr, usize)> = Vec::new();
    let record = |table: &Option<String>, name: &str, out: &mut Vec<(Expr, usize)>| {
        if shadowed(table.as_deref(), name, sides, ctx, cq) {
            return;
        }
        // Ambiguous against the outer scope (joined outer sides):
        // leave it alone, the inner run reports the ambiguity.
        if let Ok(idx) = scope.resolve_checked(table.as_deref(), name) {
            if !out.iter().any(|(_, i)| *i == idx) {
                out.push((
                    Expr::Col {
                        table: table.clone(),
                        name: name.to_string(),
                    },
                    idx,
                ));
            }
        }
    };
    let mut f = |table: &Option<String>, name: &str| -> Option<Expr> {
        record(table, name, &mut out);
        None
    };
    let _ = map_compound(cq, &mut f);
    out
}

/// Rebuild the subquery with every outer reference replaced by its
/// bound literal (one query clone per distinct binding). The rewrite
/// is gated by the SAME shadow predicate as `outer_refs`: a name the
/// inner scope owns -- even when it also resolves against the outer
/// scope, and even when it shares that outer index with a recorded
/// candidate -- must stay a column reference, never become the outer
/// row's literal.
fn substitute_refs(
    cq: &CompoundQuery,
    scope: &FromScope,
    sides: &[ShadowSide],
    ctx: &SubqCtx<'_>,
    cands: &[(Expr, usize)],
    key: &[Value],
) -> CompoundQuery {
    let mut f = |table: &Option<String>, name: &str| -> Option<Expr> {
        if shadowed(table.as_deref(), name, sides, ctx, cq) {
            return None;
        }
        let idx = scope.resolve_checked(table.as_deref(), name).ok()?;
        let slot = cands.iter().position(|(_, i)| *i == idx)?;
        Some(Expr::Lit(key[slot].clone()))
    };
    map_compound(cq, &mut f)
}
