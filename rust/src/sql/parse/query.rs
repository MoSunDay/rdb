//! SELECT translation: sqlparser Query -> IR Query.

use sqlparser::ast::{OrderByKind, Query as SqlQuery, SelectItem as SqlSelectItem, SetExpr};

use crate::sql::parse::ast::*;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::parse::table::translate_table_with_joins;
use crate::sql::parse::translate::{translate_expr, translate_limit, translate_order};

pub(crate) fn translate_query(q: &SqlQuery) -> SqlResult<Query> {
    let SetExpr::Select(sel) = q.body.as_ref() else {
        return Err(SqlError::unsupported("query bodies other than SELECT"));
    };
    if q.with.is_some() {
        return Err(SqlError::unsupported("CTE (WITH)"));
    }
    if !q.locks.is_empty() && !q.locks.iter().all(|l| l.nonblock.is_none()) {
        return Err(SqlError::unsupported("FOR UPDATE NOWAIT / SKIP LOCKED"));
    }
    let items: Vec<SelectItem> = sel
        .projection
        .iter()
        .map(translate_item)
        .collect::<SqlResult<Vec<_>>>()?;
    if sel.from.len() != 1 {
        return Err(SqlError::unsupported("comma cross joins / missing FROM"));
    }
    let from = translate_table_with_joins(&sel.from[0])?;
    let group_by = match &sel.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs, _) => exprs
            .iter()
            .map(translate_expr)
            .collect::<SqlResult<Vec<_>>>()?,
        sqlparser::ast::GroupByExpr::All(_) => return Err(SqlError::unsupported("GROUP BY ALL")),
    };
    let distinct = matches!(&sel.distinct, Some(sqlparser::ast::Distinct::Distinct));
    let for_update = q
        .locks
        .iter()
        .any(|l| matches!(l.lock_type, sqlparser::ast::LockType::Update));
    let (limit, offset) = translate_limit_clause(&q.limit_clause)?;
    Ok(Query {
        items,
        from,
        filter: sel.selection.as_ref().map(translate_expr).transpose()?,
        group_by,
        having: sel.having.as_ref().map(translate_expr).transpose()?,
        order_by: match &q.order_by {
            None => Vec::new(),
            Some(ob) => match &ob.kind {
                OrderByKind::Expressions(exprs) => translate_order(exprs)?,
                OrderByKind::All(_) => return Err(SqlError::unsupported("ORDER BY ALL")),
            },
        },
        limit,
        offset,
        distinct,
        for_update,
    })
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
