//! ORDER BY / GROUP BY key resolution: bare integer ordinals become
//! the projected expression at that position (out-of-range is MySQL's
//! ER 1054), and bare identifiers may resolve to select-list aliases.
//! Split out of `order_limit.rs` (file-size budget).

use sqlparser::ast::{Expr as SqlExpr, OrderByExpr};

use crate::sql::parse::ast::{
    CompoundQuery, CorrelatedKind, Expr, LimitValue, OrderKey, Query, QueryBody, SelectItem,
    TableRef,
};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::parse::translate::translate_expr;

/// A bare unsigned integer literal (`ORDER BY 2`) is the 1-based
/// output-column position, per MySQL. Composed (`1+1`), signed (`-1`)
/// and quoted (`'1'`) numbers are ordinary expressions, not positions.
/// Which clause a bad ordinal is reported against (MySQL wording).
const ORDER_CLAUSE: &str = "order clause";
const GROUP_CLAUSE: &str = "group statement";

fn ordinal(e: &SqlExpr) -> Option<u64> {
    if let SqlExpr::Value(v) = e {
        if let sqlparser::ast::Value::Number(n, _) = &v.value {
            // Fractions / exponents fail the u64 parse -> not a
            // position, fall through to normal expression handling.
            return n.parse::<u64>().ok();
        }
    }
    None
}

/// Where ordinal `pos` lands in the select list.
enum Hit<'a> {
    /// The explicit projection at that position.
    At(&'a Expr),
    /// The position falls inside (or after) a `*`, whose width is
    /// only known at execution time.
    Wildcard,
    /// Past the last output column.
    Miss,
}

fn projection_at(items: &[SelectItem], pos: u64) -> Hit<'_> {
    let mut seen = 0u64;
    let mut wildcard = false;
    for item in items {
        match item {
            SelectItem::Expr { expr, .. } => {
                seen += 1;
                if seen == pos {
                    return Hit::At(expr);
                }
            }
            SelectItem::Wildcard => wildcard = true,
        }
    }
    if wildcard && pos > seen {
        Hit::Wildcard
    } else {
        Hit::Miss
    }
}

/// Whether any `?` rides inside the expression. Substituting a
/// projection into ORDER BY / HAVING duplicates the parameter, which
/// would break positional binding counts -- reject loudly instead.
fn has_placeholder(e: &Expr) -> bool {
    match e {
        Expr::Placeholder => true,
        Expr::Lit(_) | Expr::Col { .. } | Expr::InsertValues(_) => false,
        Expr::Subquery(cq) => compound_has_placeholder(cq),
        Expr::InSubquery { expr, query, .. } => {
            has_placeholder(expr) || compound_has_placeholder(query)
        }
        Expr::Exists { query, .. } => compound_has_placeholder(query),
        // Bind-time node; placeholders inside bound keys/lhs are
        // already literals.
        Expr::Correlated { kind, keys, .. } => {
            keys.iter().any(has_placeholder)
                || matches!(kind, CorrelatedKind::In { lhs, .. } if has_placeholder(lhs))
        }
        Expr::BinaryOp { left, right, .. } => has_placeholder(left) || has_placeholder(right),
        Expr::Not(x) | Expr::Neg(x) => has_placeholder(x),
        Expr::IsNull { expr, .. } => has_placeholder(expr),
        Expr::InList { expr, list, .. } => {
            has_placeholder(expr) || list.iter().any(has_placeholder)
        }
        Expr::Between {
            expr, low, high, ..
        } => has_placeholder(expr) || has_placeholder(low) || has_placeholder(high),
        Expr::Like { expr, pattern, .. } => has_placeholder(expr) || has_placeholder(pattern),
        Expr::Agg { arg, .. } => arg.as_deref().is_some_and(has_placeholder),
        Expr::Func { args, .. } => args.iter().any(has_placeholder),
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            operand.as_deref().is_some_and(has_placeholder)
                || branches
                    .iter()
                    .any(|(c, t)| has_placeholder(c) || has_placeholder(t))
                || else_expr.as_deref().is_some_and(has_placeholder)
        }
        Expr::Cast { expr, .. } => has_placeholder(expr),
        Expr::Regexp { expr, pattern, .. } => has_placeholder(expr) || has_placeholder(pattern),
    }
}

fn resolve_ordinal(items: &[SelectItem], n: u64, clause: &str) -> SqlResult<Expr> {
    match projection_at(items, n) {
        Hit::At(e) if !has_placeholder(e) => Ok(e.clone()),
        Hit::At(_) => Err(SqlError::unsupported(format!(
            "positional parameters in a projection referenced by a {clause} ordinal"
        ))),
        Hit::Wildcard => Err(SqlError::unsupported(format!(
            "{clause} position {n} over a * projection (width unknown before execution)"
        ))),
        Hit::Miss => Err(SqlError::new(
            ErrorCode::BadField,
            format!("Unknown column '{n}' in '{clause}'"),
        )),
    }
}

/// Placeholder reachability through nested query shapes (substituting
/// a projection containing a subquery parameter would duplicate the
/// parameter and desync positional binding).
fn limit_has_placeholder(l: &Option<LimitValue>) -> bool {
    matches!(l.as_ref(), Some(LimitValue::Param(_)))
}

fn query_has_placeholder(q: &Query) -> bool {
    q.items.iter().any(|i| match i {
        SelectItem::Expr { expr, .. } => has_placeholder(expr),
        SelectItem::Wildcard => false,
    }) || q.filter.as_ref().is_some_and(has_placeholder)
        || q.group_by.iter().any(has_placeholder)
        || q.having.as_ref().is_some_and(has_placeholder)
        || q.order_by.iter().any(|k| has_placeholder(&k.expr))
        || limit_has_placeholder(&q.limit)
        || limit_has_placeholder(&Some(q.offset.clone()))
}

fn body_has_placeholder(b: &QueryBody) -> bool {
    match b {
        QueryBody::Select(q) => query_has_placeholder(q) || from_has_placeholder(&q.from),
        QueryBody::Nested(inner) => compound_has_placeholder(inner),
        QueryBody::SetOp { left, right, .. } => {
            body_has_placeholder(left) || body_has_placeholder(right)
        }
    }
}

fn from_has_placeholder(t: &TableRef) -> bool {
    match t {
        TableRef::Table { .. } | TableRef::NoTable => false,
        TableRef::Derived { query, .. } => compound_has_placeholder(query),
        TableRef::Join { left, right, .. } => {
            from_has_placeholder(left) || from_has_placeholder(right)
        }
    }
}

fn compound_has_placeholder(cq: &CompoundQuery) -> bool {
    cq.ctes.iter().any(|c| compound_has_placeholder(&c.query))
        || body_has_placeholder(&cq.body)
        || cq.order_by.iter().any(|k| has_placeholder(&k.expr))
        || limit_has_placeholder(&cq.limit)
        || limit_has_placeholder(&Some(cq.offset.clone()))
}

/// `(alias, projected expr)` pairs of the select list, in order.
fn alias_pairs(items: &[SelectItem]) -> Vec<(&str, &Expr)> {
    items
        .iter()
        .filter_map(|i| match i {
            SelectItem::Expr {
                alias: Some(a),
                expr,
                ..
            } => Some((a.as_str(), expr)),
            _ => None,
        })
        .collect()
}

/// Replace bare unqualified column refs naming a select-list alias
/// with the aliased projection (case-insensitive, like column
/// resolution elsewhere). MySQL resolves ORDER BY / HAVING names
/// against the select list first, then the FROM scope: substituting
/// unconditionally bakes in that alias-wins preference (a name that
/// matches both an alias and a FROM column uses the alias). The
/// replacement is inserted as-is -- no re-substitution, so aliases
/// can neither chain nor loop; on duplicate aliases the first wins.
fn substitute_aliases(e: Expr, aliases: &[(&str, &Expr)]) -> SqlResult<Expr> {
    if aliases.is_empty() {
        return Ok(e);
    }
    Ok(match e {
        Expr::Col { table: None, name } => match aliases
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(&name))
        {
            Some((_, expr)) if !has_placeholder(expr) => (*expr).clone(),
            Some(_) => return Err(SqlError::unsupported(
                "positional parameters in a projection referenced by an ORDER BY / HAVING alias",
            )),
            None => Expr::Col { table: None, name },
        },
        Expr::Col { table, name } => Expr::Col { table, name },
        Expr::Lit(_) | Expr::Placeholder | Expr::InsertValues(_) => e,
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(substitute_aliases(*left, aliases)?),
            op,
            right: Box::new(substitute_aliases(*right, aliases)?),
        },
        Expr::Not(x) => Expr::Not(Box::new(substitute_aliases(*x, aliases)?)),
        Expr::Neg(x) => Expr::Neg(Box::new(substitute_aliases(*x, aliases)?)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            list: list
                .into_iter()
                .map(|x| substitute_aliases(x, aliases))
                .collect::<SqlResult<Vec<_>>>()?,
            negated,
        },
        Expr::Subquery(_) | Expr::Exists { .. } => e,
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            query,
            negated,
        },
        // Bind-time node; alias substitution happens pre-bind.
        Expr::Correlated { .. } => e,
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            low: Box::new(substitute_aliases(*low, aliases)?),
            high: Box::new(substitute_aliases(*high, aliases)?),
            negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            pattern: Box::new(substitute_aliases(*pattern, aliases)?),
            negated,
        },
        Expr::Agg {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Agg {
            func,
            arg: arg
                .map(|a| substitute_aliases(*a, aliases).map(Box::new))
                .transpose()?,
            distinct,
            sep,
        },
        Expr::Func { name, args } => Expr::Func {
            name,
            args: args
                .into_iter()
                .map(|a| substitute_aliases(a, aliases))
                .collect::<SqlResult<Vec<_>>>()?,
        },
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => Expr::Case {
            operand: operand
                .map(|o| substitute_aliases(*o, aliases).map(Box::new))
                .transpose()?,
            branches: branches
                .into_iter()
                .map(|(c, t)| {
                    Ok((
                        substitute_aliases(c, aliases)?,
                        substitute_aliases(t, aliases)?,
                    ))
                })
                .collect::<SqlResult<Vec<_>>>()?,
            else_expr: else_expr
                .map(|e| substitute_aliases(*e, aliases).map(Box::new))
                .transpose()?,
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            to,
        },
        Expr::Regexp {
            expr,
            pattern,
            negated,
        } => Expr::Regexp {
            expr: Box::new(substitute_aliases(*expr, aliases)?),
            pattern: Box::new(substitute_aliases(*pattern, aliases)?),
            negated,
        },
    })
}

/// ORDER BY keys of a plain SELECT: bare integer literals become the
/// projection at that position (MySQL ordinal); bare identifiers (at
/// any depth) naming a select alias become the aliased projection.
/// The compound-query tail keeps its own exec-time resolution against
/// the set-operation output (`set_ops::apply_tail`).
pub(crate) fn translate_order_for_select(
    keys: &[OrderByExpr],
    items: &[SelectItem],
) -> SqlResult<Vec<OrderKey>> {
    let aliases = alias_pairs(items);
    keys.iter()
        .map(|k| {
            let expr = match ordinal(&k.expr) {
                Some(n) => resolve_ordinal(items, n, ORDER_CLAUSE)?,
                None => substitute_aliases(translate_expr(&k.expr)?, &aliases)?,
            };
            Ok(OrderKey {
                expr,
                asc: k.options.asc.unwrap_or(true),
            })
        })
        .collect()
}

/// GROUP BY keys: ordinals resolve exactly like ORDER BY's (against
/// the select list); other expressions translate unchanged -- alias
/// references in GROUP BY are a documented v1 gap.
pub(crate) fn translate_group_for_select(
    exprs: &[SqlExpr],
    items: &[SelectItem],
) -> SqlResult<Vec<Expr>> {
    exprs
        .iter()
        .map(|e| match ordinal(e) {
            Some(n) => resolve_ordinal(items, n, GROUP_CLAUSE),
            None => translate_expr(e),
        })
        .collect()
}

/// HAVING predicate with select-list aliases substituted (MySQL lets
/// HAVING reference output columns, e.g. `HAVING cnt > 1`).
pub(crate) fn translate_having(e: &SqlExpr, items: &[SelectItem]) -> SqlResult<Expr> {
    substitute_aliases(translate_expr(e)?, &alias_pairs(items))
}

#[cfg(test)]
#[path = "order_limit_tests.rs"]
mod tests;
