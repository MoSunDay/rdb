//! Subquery pre-materialization (and correlated deferral).
//!
//! `eval` works row-by-row over a materialized [`Source`] and has no
//! storage access, so subqueries are hoisted out of the expression
//! trees before evaluation starts: scalar subqueries become literals,
//! `IN (SELECT ...)` becomes an `InList`, and `EXISTS (...)` becomes a
//! boolean literal. Correlated references (columns that resolve
//! against the outer query's scope) cannot fold; the node is deferred
//! and `exec::correlated` binds it per outer row after the outer FROM
//! materializes (`SubqCtx::outer` switches this pass from defer to
//! bind).

use crate::sql::exec::relation::CteScope;
use crate::sql::exec::set_ops::run_compound;
use crate::sql::parse::ast::{Expr, OrderKey, Query, SelectItem};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;
use crate::sql::tx::Txn;
use crate::state::Shared;

/// Everything a subquery needs to materialize itself: the snapshot
/// the outer query reads at, the txn's staged writes, and the CTEs
/// visible at this point of the statement.
pub struct SubqCtx<'a> {
    pub shared: &'a Shared,
    pub read_ts: u64,
    pub txn: Option<&'a Txn>,
    pub ctes: &'a CteScope,
    /// Outer-row binding for the correlated bind pass; `None` in the
    /// pre-materialization pass (correlated nodes defer).
    pub outer: Option<OuterRows<'a>>,
}

/// The outer query's materialized rows and their resolution scope:
/// what correlated subquery references resolve against.
#[derive(Clone, Copy)]
pub struct OuterRows<'a> {
    pub scope: &'a crate::sql::exec::scan::FromScope,
    pub rows: &'a [Vec<Value>],
}

/// Materialize a subquery body, translating the plain executor's
/// unknown-column failure into the correlation verdict: a name that
/// resolves in neither the subquery's own scope nor the CTEs is, in
/// practice, an outer reference (typos still name the column).
async fn run_subquery(
    cq: &crate::sql::parse::ast::CompoundQuery,
    ctx: &SubqCtx<'_>,
) -> SqlResult<crate::sql::exec::relation::Relation> {
    Box::pin(run_compound(ctx.shared, ctx.read_ts, ctx.txn, cq, ctx.ctes))
        .await
        .map_err(|e| {
            // Only the executor's own resolution failure marks a correlation:
            // a nested subquery's already-rewritten verdict (NotSupported) must
            // not be re-wrapped, and ambiguous-name errors are not outer refs.
            if e.code == ErrorCode::BadField && e.msg.starts_with("unknown column") {
                SqlError::new(
                    ErrorCode::NotSupported,
                    format!(
                        "correlated reference not supported in this position (outer reference; {})",
                        e.msg
                    ),
                )
            } else {
                e
            }
        })
}

/// Rewrite one expression tree: every uncorrelated subquery node is
/// evaluated once and replaced by its (constant) result; a correlated
/// node defers in the pre-materialization pass and binds per outer
/// row in the bind pass (`SubqCtx::outer`).
pub async fn rewrite_expr(e: &Expr, ctx: &SubqCtx<'_>) -> SqlResult<Expr> {
    Ok(match e {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists { .. } => {
            match ctx.outer {
                // Bind pass: pre-compute per distinct outer binding.
                Some(outer) => {
                    Box::pin(crate::sql::exec::correlated::bind_node(e, outer, ctx)).await?
                }
                // Pre-materialization: fold uncorrelated nodes, defer
                // the correlation verdict for the bind pass.
                None => match materialize_uncorrelated(e, ctx).await {
                    Ok(rewritten) => rewritten,
                    Err(err) if is_correlation(&err) => e.clone(),
                    Err(err) => return Err(err),
                },
            }
        }
        // Already bound in an earlier bind pass.
        Expr::Correlated { .. } => e.clone(),
        // VALUES(col) markers live only in ODKU assignments, which the
        // subquery rewriter never sees; pass them through untouched.
        Expr::InsertValues(_) => e.clone(),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(Box::pin(rewrite_expr(left, ctx)).await?),
            op: *op,
            right: Box::new(Box::pin(rewrite_expr(right, ctx)).await?),
        },
        Expr::Not(inner) => Expr::Not(Box::new(Box::pin(rewrite_expr(inner, ctx)).await?)),
        Expr::Neg(inner) => Expr::Neg(Box::new(Box::pin(rewrite_expr(inner, ctx)).await?)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
            list: rewrite_all(list, ctx).await?,
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
            low: Box::new(Box::pin(rewrite_expr(low, ctx)).await?),
            high: Box::new(Box::pin(rewrite_expr(high, ctx)).await?),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
            pattern: Box::new(Box::pin(rewrite_expr(pattern, ctx)).await?),
            negated: *negated,
        },
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => Expr::Case {
            operand: match operand {
                Some(o) => Some(Box::new(Box::pin(rewrite_expr(o, ctx)).await?)),
                None => None,
            },
            branches: rewrite_pairs(branches, ctx).await?,
            else_expr: match else_expr {
                Some(e) => Some(Box::new(Box::pin(rewrite_expr(e, ctx)).await?)),
                None => None,
            },
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
            to: *to,
        },
        Expr::Regexp {
            expr,
            pattern,
            negated,
        } => Expr::Regexp {
            expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
            pattern: Box::new(Box::pin(rewrite_expr(pattern, ctx)).await?),
            negated: *negated,
        },
        Expr::Agg {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Agg {
            func: *func,
            arg: match arg {
                Some(a) => Some(Box::new(Box::pin(rewrite_expr(a, ctx)).await?)),
                None => None,
            },
            distinct: *distinct,
            sep: sep.clone(),
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: rewrite_all(args, ctx).await?,
        },
        // Leaf shapes carry no subqueries.
        Expr::Col { .. } | Expr::Lit(_) | Expr::Placeholder => e.clone(),
    })
}

/// Evaluate one uncorrelated subquery node to its constant shape:
/// scalar -> literal, `IN` -> `InList`, `EXISTS` -> boolean literal.
/// Errors verbatim (the correlation mapping happened in
/// [`run_subquery`]).
pub(crate) async fn materialize_uncorrelated(e: &Expr, ctx: &SubqCtx<'_>) -> SqlResult<Expr> {
    match e {
        Expr::Subquery(cq) => {
            let rel = run_subquery(cq, ctx).await?;
            Ok(Expr::Lit(scalar_of(&rel)?))
        }
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => {
            let rel = run_subquery(query, ctx).await?;
            if rel.columns.len() != 1 {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!(
                        "IN subquery must yield exactly one column (got {})",
                        rel.columns.len()
                    ),
                ));
            }
            Ok(Expr::InList {
                expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
                list: rows_literals(&rel),
                negated: *negated,
            })
        }
        Expr::Exists { query, negated } => {
            // limit-1 semantics: EXISTS is true iff the subquery
            // yields at least one row.
            let rel = run_subquery(query, ctx).await?;
            let any = !rel.rows.is_empty();
            Ok(Expr::Lit(Value::Bool(if *negated { !any } else { any })))
        }
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("{other:?} is not a subquery node"),
        )),
    }
}

/// Whether an error is the executor's correlation verdict (mapped by
/// [`run_subquery`]): the signal to defer / bind instead of failing.
pub(crate) fn is_correlation(e: &SqlError) -> bool {
    e.code == ErrorCode::NotSupported
        && e.msg
            .starts_with("correlated reference not supported in this position")
}

async fn rewrite_all(exprs: &[Expr], ctx: &SubqCtx<'_>) -> SqlResult<Vec<Expr>> {
    futures::future::try_join_all(exprs.iter().map(|e| Box::pin(rewrite_expr(e, ctx)))).await
}

/// Rewrite the (WHEN, THEN) pairs of a CASE in place.
async fn rewrite_pairs(
    branches: &[(Expr, Expr)],
    ctx: &SubqCtx<'_>,
) -> SqlResult<Vec<(Expr, Expr)>> {
    futures::future::try_join_all(branches.iter().map(|(c, t)| async {
        Ok::<_, SqlError>((
            Box::pin(rewrite_expr(c, ctx)).await?,
            Box::pin(rewrite_expr(t, ctx)).await?,
        ))
    }))
    .await
}

/// Rewrite every expression of a plain SELECT body in place.
pub async fn rewrite_query(q: &Query, ctx: &SubqCtx<'_>) -> SqlResult<Query> {
    let mut out = q.clone();
    out.filter = rewrite_opt(&q.filter, ctx).await?;
    out.items = futures::future::try_join_all(q.items.iter().map(|it| async {
        Ok::<_, SqlError>(match it {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: rewrite_expr(expr, ctx).await?,
                alias: alias.clone(),
            },
        })
    }))
    .await?;
    out.group_by = rewrite_all(&q.group_by, ctx).await?;
    out.having = rewrite_opt(&q.having, ctx).await?;
    out.order_by = futures::future::try_join_all(q.order_by.iter().map(|k| async {
        Ok::<_, SqlError>(OrderKey {
            expr: rewrite_expr(&k.expr, ctx).await?,
            asc: k.asc,
        })
    }))
    .await?;
    out.from = rewrite_from(&q.from, ctx).await?;
    Ok(out)
}

/// Rewrite JOIN conditions in place. A subquery that stays correlated
/// there cannot bind: join conditions evaluate during FROM
/// materialization, before any outer rows exist -- reject loudly.
async fn rewrite_from(
    t: &crate::sql::parse::ast::TableRef,
    ctx: &SubqCtx<'_>,
) -> SqlResult<crate::sql::parse::ast::TableRef> {
    use crate::sql::parse::ast::TableRef;
    Ok(match t {
        TableRef::Join {
            left,
            right,
            kind,
            on,
            using,
        } => {
            let on = match on {
                Some(e) => {
                    let rewritten = Box::pin(rewrite_expr(e, ctx)).await?;
                    if crate::sql::exec::correlated::expr_has_subquery(&rewritten) {
                        return Err(SqlError::new(
                            ErrorCode::NotSupported,
                            "correlated subqueries are not supported in JOIN conditions",
                        ));
                    }
                    Some(rewritten)
                }
                None => None,
            };
            TableRef::Join {
                left: Box::new(Box::pin(rewrite_from(left, ctx)).await?),
                right: Box::new(Box::pin(rewrite_from(right, ctx)).await?),
                kind: *kind,
                on,
                using: using.clone(),
            }
        }
        other => other.clone(),
    })
}

async fn rewrite_opt(e: &Option<Expr>, ctx: &SubqCtx<'_>) -> SqlResult<Option<Expr>> {
    match e {
        Some(e) => Ok(Some(rewrite_expr(e, ctx).await?)),
        None => Ok(None),
    }
}

/// Scalar-subquery semantics: one column, at most one row (empty ->
/// NULL, more -> MySQL 1242).
pub(crate) fn scalar_of(rel: &crate::sql::exec::relation::Relation) -> SqlResult<Value> {
    if rel.columns.len() != 1 {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "scalar subquery must yield exactly one column (got {})",
                rel.columns.len()
            ),
        ));
    }
    match rel.rows.len() {
        0 => Ok(Value::Null),
        1 => Ok(rel.rows[0][0].clone()),
        _ => Err(SqlError::new(
            ErrorCode::NotSupported,
            "Subquery returns more than 1 row",
        )),
    }
}

pub(crate) fn rows_literals(rel: &crate::sql::exec::relation::Relation) -> Vec<Expr> {
    rel.rows.iter().map(|r| Expr::Lit(r[0].clone())).collect()
}

#[cfg(test)]
#[path = "subquery_tests.rs"]
mod subquery_tests;
