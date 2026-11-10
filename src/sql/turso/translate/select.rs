//! SELECT rendering for the turso engine: the projection / WHERE /
//! GROUP BY + HAVING / ORDER BY / LIMIT trunk plus (since the
//! allowlist batch) FROM join trees via `join.rs`. DISTINCT and
//! locking clauses still reject with the uniform 1235 -- the
//! renderer IS the allowlist gate, nothing falls through silently.

use crate::sql::exec::render;
use crate::sql::parse::ast::{LimitValue, OrderKey, Query, SelectItem, TableRef};
use crate::sql::parse::error::SqlResult;
use crate::sql::parse::order_limit::limit_u64;

use super::super::engine;
use super::expr::render_expr;

/// Render one SELECT as a SQLite statement. Every output column gets
/// an explicit `AS "name"` alias (MySQL display name via the host
/// renderer, or the user's alias), so turso's `column_names()` doubles
/// as honest result metadata: `SELECT a AS x, b` reports x,b exactly
/// like the host engine's output_name().
pub fn render_select(q: &Query) -> SqlResult<String> {
    if q.distinct {
        return Err(engine::turso_unsupported("SELECT DISTINCT"));
    }
    if q.lock.is_some() {
        return Err(engine::turso_unsupported(
            "SELECT with a locking clause (FOR UPDATE / FOR SHARE)",
        ));
    }
    let mut items = Vec::with_capacity(q.items.len());
    for item in &q.items {
        match item {
            SelectItem::Expr { expr, alias } => {
                let rendered = render_expr(expr)?;
                let name = alias.clone().unwrap_or_else(|| render::expr_display(expr));
                items.push(format!("{rendered} AS \"{}\"", name.replace('"', "\"\"")));
            }
            // `*` only makes sense with a FROM: turso's column_names()
            // then reports the table's real columns, which is exactly
            // the honest metadata we want.
            SelectItem::Wildcard => {
                if matches!(q.from, TableRef::NoTable) {
                    return Err(engine::turso_unsupported("SELECT *"));
                }
                items.push("*".to_string());
            }
        }
    }
    let from = match &q.from {
        TableRef::NoTable => String::new(),
        // Base tables, aliases and the whole join tree (this batch)
        // render through the join renderer; it keeps its own
        // allowlist (derived tables stay 1235).
        other => format!(" FROM {}", super::join::render_table_ref(other)?),
    };
    let mut sql = format!("SELECT {}{}", items.join(", "), from);
    if let Some(f) = &q.filter {
        sql.push_str(&format!(" WHERE {}", render_expr(f)?));
    }
    if !q.group_by.is_empty() {
        let keys = q
            .group_by
            .iter()
            .map(render_expr)
            .collect::<SqlResult<Vec<_>>>()?
            .join(", ");
        sql.push_str(&format!(" GROUP BY {keys}"));
    }
    if let Some(h) = &q.having {
        sql.push_str(&format!(" HAVING {}", render_expr(h)?));
    }
    sql.push_str(&render_tail(&q.order_by, q.limit.as_ref(), &q.offset)?);
    Ok(sql)
}

/// Trailing ORDER BY / LIMIT / OFFSET, shared by the plain SELECT
/// trunk and the compound renderer (`set.rs`): the outermost query
/// owns these in both IRs, inner compound operands carry none.
pub(super) fn render_tail(
    order_by: &[OrderKey],
    limit: Option<&LimitValue>,
    offset: &LimitValue,
) -> SqlResult<String> {
    let mut sql = String::new();
    // The parser already resolved ordinals and select aliases into
    // plain expressions, so keys render as-is.
    if !order_by.is_empty() {
        let keys = order_by
            .iter()
            .map(|k| {
                Ok(format!(
                    "{} {}",
                    render_expr(&k.expr)?,
                    if k.asc { "ASC" } else { "DESC" }
                ))
            })
            .collect::<SqlResult<Vec<_>>>()?
            .join(", ");
        sql.push_str(&format!(" ORDER BY {keys}"));
    }
    // MySQL's `LIMIT o, n` was normalized into (limit=n, offset=o) at
    // parse time; SQLite spells the same shape LIMIT n OFFSET o.
    let offset = limit_u64(offset)?;
    match limit {
        Some(l) => {
            let n = limit_u64(l)?;
            sql.push_str(&format!(" LIMIT {n} OFFSET {offset}"));
        }
        // OFFSET without LIMIT is legal in the IR (MySQL needs the
        // huge-LIMIT trick; SQLite uses the -1 sentinel).
        None if offset > 0 => sql.push_str(&format!(" LIMIT -1 OFFSET {offset}")),
        None => {}
    }
    Ok(sql)
}
