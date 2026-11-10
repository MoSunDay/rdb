//! Compound-query rendering for the turso engine: WITH prefixes,
//! the set-operation tree (UNION / UNION ALL / INTERSECT / EXCEPT)
//! and the trailing ORDER BY / LIMIT the outermost query owns. Also
//! the body renderer every expression subquery rides (`expr.rs`
//! hands its `CompoundQuery` here). Turso facts verified against
//! 0.9.0-pre.2 with the scratch smoke binary before wiring:
//!   * compound operands CANNOT be parenthesized ("near \"(\": syntax
//!     error") -- `QueryBody::Nested` (an explicit parenthesized
//!     operand) therefore renders as a derived-table wrap
//!     `SELECT * FROM (...)`, which both forces the parsed tree
//!     shape and is plain SQLite;
//!   * mixed chains left-fold at equal precedence, exactly like the
//!     rdb AST's sqlparser-built tree, so unparenthesized chains
//!     render flat in parsed order;
//!   * `INTERSECT ALL` / `EXCEPT ALL` are not in the grammar (only
//!     UNION takes ALL) -- those keep the uniform 1235 while rocks
//!     answers them (multiset semantics), a matrix note.

use crate::sql::parse::ast::{CompoundQuery, QueryBody, SetOp};
use crate::sql::parse::error::SqlResult;

use super::super::engine;
use super::quote_ident;
use super::select::{render_select, render_tail};

/// Render one full query expression (CTEs + body + trailing ORDER BY
/// / LIMIT) as SQLite SQL. Used by the statement router for
/// `Statement::SelectCompound` and recursively for CTE bodies,
/// parenthesized operands and every expression subquery.
pub fn render_compound(cq: &CompoundQuery) -> SqlResult<String> {
    let mut sql = String::new();
    if !cq.ctes.is_empty() {
        // RECURSIVE already rejected at parse; each definition sees
        // the earlier ones (same order both engines).
        let defs = cq
            .ctes
            .iter()
            .map(|c| {
                let cols = if c.column_aliases.is_empty() {
                    String::new()
                } else {
                    let names = c
                        .column_aliases
                        .iter()
                        .map(|a| quote_ident(a))
                        .collect::<Vec<_>>();
                    format!(" ({})", names.join(", "))
                };
                Ok(format!(
                    "{}{} AS ({})",
                    quote_ident(&c.name),
                    cols,
                    render_compound(&c.query)?
                ))
            })
            .collect::<SqlResult<Vec<_>>>()?
            .join(", ");
        sql.push_str(&format!("WITH {defs} "));
    }
    sql.push_str(&render_body(&cq.body)?);
    // Column naming parity needs no work here: SQLite names a
    // compound after its FIRST branch, like MySQL/rocks, and each
    // branch's outputs carry explicit AS aliases already.
    sql.push_str(&render_tail(&cq.order_by, cq.limit.as_ref(), &cq.offset)?);
    Ok(sql)
}

/// One set-operation operand: a plain SELECT, a parenthesized full
/// query, or a binary set operation.
fn render_body(b: &QueryBody) -> SqlResult<String> {
    match b {
        QueryBody::Select(q) => render_select(q),
        // A parenthesized operand is a parse error in SQLite's
        // compound grammar, so the parenthesized subtree rides a
        // derived table instead -- same rows, same first-branch
        // names, tree shape preserved verbatim.
        QueryBody::Nested(inner) => Ok(format!("SELECT * FROM ({})", render_compound(inner)?)),
        QueryBody::SetOp {
            op,
            left,
            right,
            all,
        } => {
            if *all && !matches!(op, SetOp::Union) {
                return Err(engine::turso_unsupported(format!(
                    "{op} ALL (the engine's grammar takes ALL on UNION only)"
                )));
            }
            Ok(format!(
                "{} {}{} {}",
                render_body(left)?,
                op,
                if *all { " ALL" } else { "" },
                render_body(right)?
            ))
        }
    }
}
