//! FROM-clause rendering for the turso engine: base tables with
//! aliases plus the join tree (INNER / LEFT / RIGHT / FULL / CROSS,
//! each with ON expr or USING column list). Every kind was
//! smoke-verified against turso 0.9.0-pre.2 before wiring (scratch
//! binary, same crate): the planner implements RIGHT and FULL
//! natively (`translate/plan.rs` JoinType::{RightOuter,FullOuter}),
//! so no kind stays on the 1235 list here. Derived tables (FROM
//! (SELECT ...)) remain the one FROM shape this renderer rejects.

use crate::sql::parse::ast::{JoinKind, TableRef};
use crate::sql::parse::error::SqlResult;

use super::super::engine;
use super::expr::render_expr;
use super::quote_ident;

/// One FROM-clause table reference. A join tree renders as its left
/// spine followed by each `JOIN` in AST order -- the tree IS the
/// associativity, and sqlparser left-folds chains, which matches
/// SQLite's own equal-precedence left-fold parser.
pub fn render_table_ref(t: &TableRef) -> SqlResult<String> {
    match t {
        TableRef::Table { name, alias } => Ok(match alias {
            Some(a) => format!("{} AS {}", quote_ident(name), quote_ident(a)),
            None => quote_ident(name),
        }),
        TableRef::Join {
            left,
            right,
            kind,
            on,
            using,
        } => {
            if on.is_some() && !using.is_empty() {
                return Err(engine::turso_unsupported("JOIN ... ON together with USING"));
            }
            let kw = match kind {
                JoinKind::Inner => "INNER JOIN",
                JoinKind::Left => "LEFT JOIN",
                JoinKind::Right => "RIGHT JOIN",
                JoinKind::Full => "FULL OUTER JOIN",
                JoinKind::Cross => "CROSS JOIN",
            };
            // ON and USING are mutually exclusive per grammar (and
            // re-checked above); both absent = unconstrained join
            // (bare `JOIN` parses as INNER with no condition, CROSS
            // normally -- sqlparser may park a non-standard
            // constraint on CROSS JOIN, which renders verbatim).
            let cond = if let Some(e) = on {
                format!(" ON {}", render_expr(e)?)
            } else if !using.is_empty() {
                let cols = using.iter().map(|c| quote_ident(c)).collect::<Vec<_>>();
                format!(" USING ({})", cols.join(", "))
            } else {
                String::new()
            };
            Ok(format!(
                "{} {} {}{}",
                render_table_ref(left)?,
                kw,
                render_table_ref(right)?,
                cond
            ))
        }
        // FROM (SELECT ...) alias: still outside the turso allowlist
        // (this batch covers joins + expression subqueries only).
        TableRef::Derived { .. } => Err(engine::turso_unsupported(
            "derived tables (FROM (SELECT ...))",
        )),
        // The caller (render_select) folds NoTable into "no FROM
        // clause"; a join operand can never be one (parse rejects).
        TableRef::NoTable => Err(engine::turso_unsupported(
            "a FROM-less SELECT in a join position",
        )),
    }
}
