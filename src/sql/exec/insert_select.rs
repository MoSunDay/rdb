//! `INSERT ... SELECT` source materialization (M2): the source compound
//! query runs to completion BEFORE any write of the statement, so
//! `INSERT INTO t SELECT ... FROM t` reads the pre-statement snapshot
//! (its read point is taken ahead of the write path's, and rows land
//! only after the whole source materialized).
//!
//! The materialized cells then flow through the SAME row-construction
//! path as VALUES tuples (`insert_common::build_row_values` called by
//! `write::insert`): column-list mapping, coercion to the column
//! types, NOT NULL and AUTO_INCREMENT handling are shared, and the
//! statement's conflict action (plain / ODKU / REPLACE) applies to
//! SELECT-sourced rows exactly like VALUES rows.
//!
//! Cluster mode: the SELECT side already scatter-gathers to the
//! caller (`exec::set_ops` -> `dist::gather`), so a plain
//! `INSERT ... SELECT` gathers once and 2PCs its rows like any
//! multi-row INSERT; only the ODKU/REPLACE conflict READ is vetoed in
//! cluster mode (see `write::insert`).

use crate::sql::exec::relation::CteScope;
use crate::sql::exec::set_ops;
use crate::sql::parse::ast::CompoundQuery;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{TableSchema, Value};
use crate::state::Shared;

use crate::sql::exec::SqlSession;

/// Materialize the source SELECT of an INSERT into cell rows: one
/// snapshot read (autocommit: frontier-synced `now()`; txn: the pinned
/// `read_ts` merged with the txn's staged writes), then a positional
/// arity check against the target column list (or the table width when
/// no list was given). Coercion and NOT NULL run later, shared with
/// the VALUES path.
pub(crate) async fn materialize(
    shared: &Shared,
    sess: &SqlSession,
    cq: &CompoundQuery,
    columns: &[String],
    schema: &TableSchema,
) -> SqlResult<Vec<Vec<Value>>> {
    let (read_ts, txn) = match sess.txn.as_ref() {
        Some(t) => (t.read_ts, Some(t)),
        None => {
            shared.sql_ts.sync_cursor_frontier();
            (shared.sql_ts.now(), None)
        }
    };
    let rel = set_ops::run_compound(shared, read_ts, txn, cq, &CteScope::default()).await?;
    let want = if columns.is_empty() {
        schema.columns.len()
    } else {
        columns.len()
    };
    if rel.columns.len() != want {
        return Err(SqlError::new(
            ErrorCode::WrongValueCount,
            format!(
                "INSERT ... SELECT column count mismatch: source has {}, expected {want}",
                rel.columns.len()
            ),
        ));
    }
    Ok(rel.rows.as_ref().clone())
}

#[cfg(test)]
#[path = "insert_select_tests.rs"]
mod tests;
