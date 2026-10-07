//! Column-reference rewriting over a compound query's OWN expression
//! positions (shared by correlated-subquery detection and binding):
//! projections, WHERE / GROUP BY / HAVING / ORDER BY and JOIN
//! conditions. Nested subquery bodies stay opaque -- each runs
//! through its own pipeline with its own outer scope.

use crate::sql::parse::ast::{
    CompoundQuery, Expr, OrderKey, Query, QueryBody, SelectItem, TableRef,
};

/// Rewrite every column reference through `f` (`None` = keep).
pub(crate) fn map_compound(
    cq: &CompoundQuery,
    f: &mut impl FnMut(&Option<String>, &str) -> Option<Expr>,
) -> CompoundQuery {
    CompoundQuery {
        ctes: cq.ctes.clone(),
        body: map_body(&cq.body, f),
        order_by: cq.order_by.clone(),
        limit: cq.limit.clone(),
        offset: cq.offset.clone(),
    }
}

fn map_body(b: &QueryBody, f: &mut impl FnMut(&Option<String>, &str) -> Option<Expr>) -> QueryBody {
    match b {
        QueryBody::Select(q) => QueryBody::Select(Box::new(map_query(q, f))),
        QueryBody::Nested(inner) => QueryBody::Nested(Box::new(map_compound(inner, f))),
        QueryBody::SetOp {
            op,
            left,
            right,
            all,
        } => QueryBody::SetOp {
            op: *op,
            left: Box::new(map_body(left, f)),
            right: Box::new(map_body(right, f)),
            all: *all,
        },
    }
}

fn map_query(q: &Query, f: &mut impl FnMut(&Option<String>, &str) -> Option<Expr>) -> Query {
    Query {
        items: q
            .items
            .iter()
            .map(|i| match i {
                SelectItem::Wildcard => SelectItem::Wildcard,
                SelectItem::Expr { expr, alias } => SelectItem::Expr {
                    expr: map_expr(expr, f),
                    alias: alias.clone(),
                },
            })
            .collect(),
        from: map_from(&q.from, f),
        filter: q.filter.as_ref().map(|e| map_expr(e, f)),
        group_by: q.group_by.iter().map(|e| map_expr(e, f)).collect(),
        having: q.having.as_ref().map(|e| map_expr(e, f)),
        order_by: q
            .order_by
            .iter()
            .map(|k| OrderKey {
                expr: map_expr(&k.expr, f),
                asc: k.asc,
            })
            .collect(),
        limit: q.limit.clone(),
        offset: q.offset.clone(),
        distinct: q.distinct,
        lock: q.lock,
    }
}

/// JOIN conditions belong to the joined row scope; table refs and
/// derived bodies are opaque.
fn map_from(t: &TableRef, f: &mut impl FnMut(&Option<String>, &str) -> Option<Expr>) -> TableRef {
    match t {
        TableRef::Join {
            left,
            right,
            kind,
            on,
            using,
        } => TableRef::Join {
            left: Box::new(map_from(left, f)),
            right: Box::new(map_from(right, f)),
            kind: *kind,
            on: on.as_ref().map(|e| map_expr(e, f)),
            using: using.clone(),
        },
        other => other.clone(),
    }
}

/// JOIN conditions belong to the joined row scope; table refs and
/// derived bodies are opaque.
fn map_expr(e: &Expr, f: &mut impl FnMut(&Option<String>, &str) -> Option<Expr>) -> Expr {
    match e {
        Expr::Col { table, name } => f(table, name).unwrap_or_else(|| e.clone()),
        Expr::Lit(_) | Expr::Placeholder | Expr::InsertValues(_) => e.clone(),
        // Subquery bodies run through their own pipelines; only an
        // IN's left side evaluates in this scope.
        Expr::Subquery(cq) => Expr::Subquery(cq.clone()),
        Expr::Exists { query, negated } => Expr::Exists {
            query: query.clone(),
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(map_expr(expr, f)),
            query: query.clone(),
            negated: *negated,
        },
        Expr::Correlated { .. } => e.clone(),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(map_expr(left, f)),
            op: *op,
            right: Box::new(map_expr(right, f)),
        },
        Expr::Not(x) => Expr::Not(Box::new(map_expr(x, f))),
        Expr::Neg(x) => Expr::Neg(Box::new(map_expr(x, f))),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(map_expr(expr, f)),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(map_expr(expr, f)),
            list: list.iter().map(|x| map_expr(x, f)).collect(),
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(map_expr(expr, f)),
            low: Box::new(map_expr(low, f)),
            high: Box::new(map_expr(high, f)),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(map_expr(expr, f)),
            pattern: Box::new(map_expr(pattern, f)),
            negated: *negated,
        },
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => Expr::Case {
            operand: operand.as_deref().map(|o| map_expr(o, f)).map(Box::new),
            branches: branches
                .iter()
                .map(|(c, t)| (map_expr(c, f), map_expr(t, f)))
                .collect(),
            else_expr: else_expr.as_deref().map(|x| map_expr(x, f)).map(Box::new),
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(map_expr(expr, f)),
            to: *to,
        },
        Expr::Regexp {
            expr,
            pattern,
            negated,
        } => Expr::Regexp {
            expr: Box::new(map_expr(expr, f)),
            pattern: Box::new(map_expr(pattern, f)),
            negated: *negated,
        },
        Expr::Agg {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Agg {
            func: *func,
            arg: arg.as_deref().map(|a| map_expr(a, f)).map(Box::new),
            distinct: *distinct,
            sep: sep.clone(),
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args.iter().map(|a| map_expr(a, f)).collect(),
        },
    }
}
