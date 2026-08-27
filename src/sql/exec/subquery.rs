//! Uncorrelated subquery pre-materialization.
//!
//! `eval` works row-by-row over a materialized [`Source`] and has no
//! storage access, so subqueries are hoisted out of the expression
//! trees before evaluation starts: scalar subqueries become literals
//! and `IN (SELECT ...)` becomes an `InList`. Correlated references
//! (columns that only resolve against the outer scope) cannot be
//! pre-materialized and are rejected loudly.

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
                        "correlated subqueries are not supported (outer reference; {})",
                        e.msg
                    ),
                )
            } else {
                e
            }
        })
}

/// Rewrite one expression tree: every subquery node is evaluated once
/// and replaced by its (constant) result.
pub async fn rewrite_expr(e: &Expr, ctx: &SubqCtx<'_>) -> SqlResult<Expr> {
    Ok(match e {
        Expr::Subquery(cq) => {
            let rel = run_subquery(cq, ctx).await?;
            Expr::Lit(scalar_of(&rel)?)
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
            Expr::InList {
                expr: Box::new(Box::pin(rewrite_expr(expr, ctx)).await?),
                list: rows_literals(&rel),
                negated: *negated,
            }
        }
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
        Expr::Agg {
            func,
            arg,
            distinct,
        } => Expr::Agg {
            func: *func,
            arg: match arg {
                Some(a) => Some(Box::new(Box::pin(rewrite_expr(a, ctx)).await?)),
                None => None,
            },
            distinct: *distinct,
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: rewrite_all(args, ctx).await?,
        },
        // Leaf shapes carry no subqueries.
        Expr::Col { .. } | Expr::Lit(_) | Expr::Placeholder => e.clone(),
    })
}

async fn rewrite_all(exprs: &[Expr], ctx: &SubqCtx<'_>) -> SqlResult<Vec<Expr>> {
    futures::future::try_join_all(exprs.iter().map(|e| Box::pin(rewrite_expr(e, ctx)))).await
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
    Ok(out)
}

async fn rewrite_opt(e: &Option<Expr>, ctx: &SubqCtx<'_>) -> SqlResult<Option<Expr>> {
    match e {
        Some(e) => Ok(Some(rewrite_expr(e, ctx).await?)),
        None => Ok(None),
    }
}

/// Scalar-subquery semantics: one column, at most one row (empty ->
/// NULL, more -> MySQL 1242).
fn scalar_of(rel: &crate::sql::exec::relation::Relation) -> SqlResult<Value> {
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

fn rows_literals(rel: &crate::sql::exec::relation::Relation) -> Vec<Expr> {
    rel.rows.iter().map(|r| Expr::Lit(r[0].clone())).collect()
}
