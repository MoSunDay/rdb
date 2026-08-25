//! SELECT translation: sqlparser Query -> IR Query.

use sqlparser::ast::{
    OrderByKind, Query as SqlQuery, SelectItem as SqlSelectItem, SetExpr, SetOperator,
    SetQuantifier,
};

use crate::sql::parse::ast::*;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::parse::table::translate_table_with_joins;
use crate::sql::parse::translate::{translate_expr, translate_limit, translate_order};

/// A full query expression: WITH + set-operation body + trailing
/// ORDER BY / LIMIT. Used for every compound shape (CTE, UNION,
/// derived-table / subquery bodies); a lock-bearing plain SELECT
/// keeps the `Statement::Select` fast path.
pub(crate) fn translate_compound(q: &SqlQuery) -> SqlResult<CompoundQuery> {
    let ctes = match &q.with {
        None => Vec::new(),
        Some(w) => {
            if w.recursive {
                return Err(SqlError::unsupported("WITH RECURSIVE"));
            }
            w.cte_tables
                .iter()
                .map(|c| {
                    let column_aliases = c
                        .alias
                        .columns
                        .iter()
                        .map(|col| col.name.value.clone())
                        .collect::<Vec<_>>();
                    Ok(Cte {
                        name: c.alias.name.value.clone(),
                        column_aliases,
                        query: Box::new(translate_compound(&c.query)?),
                    })
                })
                .collect::<SqlResult<Vec<_>>>()?
        }
    };
    // Locks ride on the outer Query and only make sense when the body
    // is a lone SELECT (a UNION's dedup/concat would blur latching).
    let locks = if matches!(q.body.as_ref(), SetExpr::Select(_)) {
        q.locks.first()
    } else {
        if !q.locks.is_empty() {
            return Err(SqlError::unsupported(
                "FOR UPDATE / FOR SHARE on set operations",
            ));
        }
        None
    };
    let body = translate_body(q.body.as_ref(), locks)?;
    let (limit, offset) = translate_limit_clause(&q.limit_clause)?;
    Ok(CompoundQuery {
        ctes,
        body,
        order_by: match &q.order_by {
            None => Vec::new(),
            Some(ob) => match &ob.kind {
                OrderByKind::Expressions(exprs) => translate_order(exprs)?,
                OrderByKind::All(_) => return Err(SqlError::unsupported("ORDER BY ALL")),
            },
        },
        limit,
        offset,
    })
}

/// One set-operation operand: a SELECT, a parenthesized full query
/// (own ORDER BY / LIMIT, no outer locks), or a UNION of two.
fn translate_body(b: &SetExpr, locks: Option<&sqlparser::ast::LockClause>) -> SqlResult<QueryBody> {
    match b {
        SetExpr::Select(sel) => {
            let mut inner = translate_select(sel)?;
            inner.lock = translate_lock(locks, &inner.from)?;
            Ok(QueryBody::Select(Box::new(inner)))
        }
        SetExpr::Query(inner) => Ok(QueryBody::Nested(Box::new(translate_compound(inner)?))),
        SetExpr::SetOperation {
            op,
            set_quantifier,
            left,
            right,
        } => {
            if !matches!(op, SetOperator::Union) {
                return Err(SqlError::unsupported(format!("{op} set operations")));
            }
            let all = match set_quantifier {
                SetQuantifier::All => true,
                SetQuantifier::None | SetQuantifier::Distinct => false,
                q => return Err(SqlError::unsupported(format!("{q:?} set quantifier"))),
            };
            Ok(QueryBody::Union {
                left: Box::new(translate_body(left, None)?),
                right: Box::new(translate_body(right, None)?),
                all,
            })
        }
        other => Err(SqlError::unsupported(format!("{other} query body"))),
    }
}

/// Core SELECT body (projection / FROM / WHERE / GROUP BY / HAVING /
/// DISTINCT); trailing ORDER BY / LIMIT / locks belong to the
/// enclosing query expression.
pub(crate) fn translate_select(sel: &sqlparser::ast::Select) -> SqlResult<Query> {
    let items: Vec<SelectItem> = sel
        .projection
        .iter()
        .map(translate_item)
        .collect::<SqlResult<Vec<_>>>()?;
    if sel.from.len() > 1 {
        return Err(SqlError::unsupported("comma cross joins"));
    }
    let from = match sel.from.first() {
        Some(twj) => translate_table_with_joins(twj)?,
        // FROM-less SELECT evaluates its items once against no rows.
        None => TableRef::NoTable,
    };
    let group_by = match &sel.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs, _) => exprs
            .iter()
            .map(translate_expr)
            .collect::<SqlResult<Vec<_>>>()?,
        sqlparser::ast::GroupByExpr::All(_) => return Err(SqlError::unsupported("GROUP BY ALL")),
    };
    let distinct = matches!(&sel.distinct, Some(sqlparser::ast::Distinct::Distinct));
    Ok(Query {
        items,
        from,
        filter: sel.selection.as_ref().map(translate_expr).transpose()?,
        group_by,
        having: sel.having.as_ref().map(translate_expr).transpose()?,
        order_by: Vec::new(),
        limit: None,
        offset: 0,
        distinct,
        lock: None,
    })
}

pub(crate) fn translate_query(q: &SqlQuery) -> SqlResult<Query> {
    let SetExpr::Select(sel) = q.body.as_ref() else {
        return Err(SqlError::unsupported("query bodies other than SELECT"));
    };
    if q.with.is_some() {
        return Err(SqlError::unsupported("CTE (WITH)"));
    }
    if q.locks.len() > 1 {
        return Err(SqlError::unsupported(
            "multiple locking clauses (FOR UPDATE / FOR SHARE)",
        ));
    }
    let mut query = translate_select(sel)?;
    query.lock = translate_lock(q.locks.first(), &query.from)?;
    let (limit, offset) = translate_limit_clause(&q.limit_clause)?;
    query.limit = limit;
    query.offset = offset;
    query.order_by = match &q.order_by {
        None => Vec::new(),
        Some(ob) => match &ob.kind {
            OrderByKind::Expressions(exprs) => translate_order(exprs)?,
            OrderByKind::All(_) => return Err(SqlError::unsupported("ORDER BY ALL")),
        },
    };
    Ok(query)
}

/// `FOR UPDATE` / `FOR SHARE` -> [`LockRead`] metadata on the Query.
/// `NOWAIT` / `SKIP LOCKED` stay rejected (the executor fails fast on
/// latch conflicts; it never waits or skips). `FOR ... OF tbl` is the
/// same lock when it names the single FROM table, anything else is
/// rejected loudly rather than half-applied.
fn translate_lock(
    lc: Option<&sqlparser::ast::LockClause>,
    from: &TableRef,
) -> SqlResult<Option<LockRead>> {
    let Some(lc) = lc else {
        return Ok(None);
    };
    if let Some(nb) = &lc.nonblock {
        return Err(SqlError::unsupported(match nb {
            sqlparser::ast::NonBlock::Nowait => "NOWAIT",
            sqlparser::ast::NonBlock::SkipLocked => "SKIP LOCKED",
        }));
    }
    if let Some(of) = &lc.of {
        let TableRef::Table { name, .. } = from else {
            return Err(SqlError::unsupported("FOR UPDATE / FOR SHARE OF in joins"));
        };
        if !of.to_string().eq_ignore_ascii_case(name) {
            return Err(SqlError::unsupported(format!(
                "FOR UPDATE / FOR SHARE OF {of} (does not name the FROM table)"
            )));
        }
    }
    Ok(Some(match lc.lock_type {
        sqlparser::ast::LockType::Update => LockRead::ForUpdate,
        sqlparser::ast::LockType::Share => LockRead::ForShare,
    }))
}

fn translate_limit_clause(
    lc: &Option<sqlparser::ast::LimitClause>,
) -> SqlResult<(Option<u64>, u64)> {
    use sqlparser::ast::LimitClause;
    match lc {
        None => Ok((None, 0)),
        Some(LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        }) => {
            if !limit_by.is_empty() {
                return Err(SqlError::unsupported("LIMIT ... BY"));
            }
            Ok((
                translate_limit(limit)?,
                offset
                    .as_ref()
                    .map(|o| translate_offset(&o.value))
                    .transpose()?
                    .unwrap_or(0),
            ))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => Ok((
            translate_limit(&Some(limit.clone()))?,
            translate_offset(offset)?,
        )),
    }
}

fn translate_offset(e: &sqlparser::ast::Expr) -> SqlResult<u64> {
    match translate_expr(e)? {
        Expr::Lit(crate::sql::storage::schema::Value::Int(n)) if n >= 0 => Ok(n as u64),
        _ => Err(SqlError::parse("OFFSET must be a non-negative integer")),
    }
}

fn translate_item(item: &SqlSelectItem) -> SqlResult<SelectItem> {
    Ok(match item {
        SqlSelectItem::UnnamedExpr(e) => SelectItem::Expr {
            expr: translate_expr(e)?,
            alias: None,
        },
        SqlSelectItem::ExprWithAlias { expr, alias } => SelectItem::Expr {
            expr: translate_expr(expr)?,
            alias: Some(alias.value.clone()),
        },
        SqlSelectItem::Wildcard(_) => SelectItem::Wildcard,
        other => return Err(SqlError::unsupported(format!("{other}"))),
    })
}
