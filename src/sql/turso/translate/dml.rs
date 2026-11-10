//! DML rendering for the turso engine: INSERT (multi-row literal
//! VALUES), UPDATE, DELETE and TRUNCATE (as an unrestricted DELETE).
//! Every unsupported shape rejects with the uniform 1235 so nothing
//! degrades silently.

use crate::sql::parse::ast::{ConflictAction, Expr, InsertSource, Statement};
use crate::sql::parse::error::SqlResult;

use super::super::engine;
use super::super::meta::TableColumns;
use super::expr::{render_expr, render_value};
use super::quote_ident;
use super::vector;

/// `INSERT INTO t [(c1, ...)] VALUES (literals), ...`: multi-row,
/// explicit column list allowed; column count/type validation is left
/// to the engine (the same errors the host executor's build_row_values
/// would raise surface as MySQL errnos via engine::map_error) -- plus
/// the app-side vector(n) check here (D6: the engine has no column
/// constraint to hang it on). AUTO_INCREMENT columns omitted from the
/// list (or given NULL) take SQLite's INTEGER PRIMARY KEY AUTOINCREMENT
/// rowid alias semantics.
///
/// `cols` = the target table's column metadata (None = unknown table:
/// no positional mapping, the engine's no-such-table error decides).
pub fn render_insert(stmt: &Statement, cols: Option<&TableColumns>) -> SqlResult<String> {
    let Statement::Insert {
        table,
        columns,
        source,
        conflict,
    } = stmt
    else {
        return Err(engine::turso_unsupported(
            "non-INSERT in the INSERT renderer",
        ));
    };
    if !matches!(conflict, ConflictAction::Error) {
        return Err(engine::turso_unsupported(
            "INSERT with an explicit conflict clause (ON DUPLICATE KEY UPDATE / REPLACE)",
        ));
    }
    let InsertSource::Values(rows) = source else {
        return Err(engine::turso_unsupported("INSERT ... SELECT"));
    };
    if rows.is_empty() {
        return Err(engine::turso_unsupported(
            "INSERT with an empty VALUES list",
        ));
    }
    let width = rows[0].len();
    let col_list = if columns.is_empty() {
        String::new()
    } else {
        format!(
            " ({})",
            columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let tuples = rows
        .iter()
        .map(|row| {
            if row.len() != width {
                return Err(engine::turso_unsupported(
                    "INSERT rows of differing column counts",
                ));
            }
            let cells: SqlResult<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| {
                    // Cell i targets the i-th name of the explicit
                    // column list, or (no list) the i-th table column.
                    let name: Option<&str> = if columns.is_empty() {
                        cols.and_then(|c| c.names.get(i).map(String::as_str))
                    } else {
                        columns.get(i).map(String::as_str)
                    };
                    // Vector-targeted cells validate + normalize to
                    // the blob spelling (D6); everything else renders
                    // as before.
                    match name.and_then(|n| cols.and_then(|c| c.dim_of(n))) {
                        Some(d) => vector::write_value(cell, d),
                        None => render_value_expr(cell),
                    }
                })
                .collect();
            Ok(format!("({})", cells?.join(", ")))
        })
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(format!(
        "INSERT INTO {}{} VALUES {}",
        quote_ident(table),
        col_list,
        tuples.join(", ")
    ))
}

/// One VALUES cell: literal values render through the shared value
/// renderer; anything the expression renderer supports rides along
/// (constant arithmetic), everything else rejects.
fn render_value_expr(e: &Expr) -> SqlResult<String> {
    match e {
        Expr::Lit(v) => Ok(render_value(v)),
        other => render_expr(other),
    }
}

/// `UPDATE t SET c = expr, ... [WHERE]`. ORDER BY / LIMIT spellings
/// need SQLite's optional UPDATE..LIMIT build flag, which turso does
/// not carry -- loud 1235 instead of a silent drop. Vector-targeted
/// assignments validate app-side exactly like INSERT cells.
pub fn render_update(stmt: &Statement, cols: Option<&TableColumns>) -> SqlResult<String> {
    let Statement::Update {
        table,
        assignments,
        filter,
        order_by,
        limit,
    } = stmt
    else {
        return Err(engine::turso_unsupported(
            "non-UPDATE in the UPDATE renderer",
        ));
    };
    if !order_by.is_empty() {
        return Err(engine::turso_unsupported("UPDATE with ORDER BY"));
    }
    if limit.is_some() {
        return Err(engine::turso_unsupported("UPDATE with LIMIT"));
    }
    let sets = assignments
        .iter()
        .map(|(col, e)| {
            let sql = match cols.and_then(|c| c.dim_of(col)) {
                Some(d) => vector::write_value(e, d)?,
                None => render_expr(e)?,
            };
            Ok(format!("{} = {}", quote_ident(col), sql))
        })
        .collect::<SqlResult<Vec<_>>>()?
        .join(", ");
    let mut sql = format!("UPDATE {} SET {}", quote_ident(table), sets);
    if let Some(f) = filter {
        sql.push_str(&format!(" WHERE {}", render_expr(f)?));
    }
    Ok(sql)
}

/// `DELETE FROM t [WHERE]` (ORDER BY / LIMIT as UPDATE above).
pub fn render_delete(stmt: &Statement) -> SqlResult<String> {
    let Statement::Delete {
        table,
        filter,
        order_by,
        limit,
    } = stmt
    else {
        return Err(engine::turso_unsupported(
            "non-DELETE in the DELETE renderer",
        ));
    };
    if !order_by.is_empty() {
        return Err(engine::turso_unsupported("DELETE with ORDER BY"));
    }
    if limit.is_some() {
        return Err(engine::turso_unsupported("DELETE with LIMIT"));
    }
    let mut sql = format!("DELETE FROM {}", quote_ident(table));
    if let Some(f) = filter {
        sql.push_str(&format!(" WHERE {}", render_expr(f)?));
    }
    Ok(sql)
}

/// `TRUNCATE t` as `DELETE FROM t`: row wipe with MySQL's zero
/// affected-rows DDL shape (the caller drops the DELETE count). The
/// AUTO_INCREMENT counter does NOT reset (turso's watermark lives in
/// its internal sequence table, not sqlite_sequence) -- a documented
/// ledger deviation from MySQL/rocks TRUNCATE.
pub fn render_truncate(table: &str) -> String {
    format!("DELETE FROM {}", quote_ident(table))
}
