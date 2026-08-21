//! FROM-clause materialization: table scans and nested-loop joins.
//!
//! A [`Source`] is a query's fully materialized input: the joined rows
//! (left side's columns first) plus the [`FromScope`] that resolves
//! column references into offsets of those rows. Scans are snapshot
//! reads: per primary key only the newest version with `ts <= read_ts`
//! is decoded, and tombstoned keys are invisible.

use std::collections::BTreeMap;

use crate::sql::exec::expr::{eval, truthy, ColumnScope};
use crate::sql::index;
use crate::sql::parse::ast::{Expr, TableRef};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::plan;
use crate::sql::storage::catalog;
use crate::sql::storage::row::{self, HEADER_LIVE};
use crate::sql::storage::schema::{SqlType, TableSchema, Value};
use crate::state::Shared;
use crate::store::ops;
use crate::store::Store;

/// One relation of a FROM scope: qualifier, column names/types, and the
/// side's offset inside the joined row slice.
#[derive(Debug, Clone)]
pub struct ScopeSide {
    /// Output qualifier: the alias when given, else the table name.
    pub qualifier: String,
    /// Underlying table name (catalog key; differs when aliased).
    pub table: String,
    pub columns: Vec<String>,
    pub types: Vec<SqlType>,
    /// Offset of this side's first column in the joined row.
    pub offset: usize,
}

/// Column-resolution scope over one or more joined tables.
#[derive(Debug, Clone, Default)]
pub struct FromScope {
    pub sides: Vec<ScopeSide>,
}

/// A materialized FROM clause: rows plus their resolution scope.
#[derive(Debug, Clone, Default)]
pub struct Source {
    pub scope: FromScope,
    pub rows: Vec<Vec<Value>>,
}

impl FromScope {
    /// Total number of columns in one joined row.
    pub fn row_width(&self) -> usize {
        self.sides
            .last()
            .map(|s| s.offset + s.columns.len())
            .unwrap_or(0)
    }

    /// SQL type of the column at `idx` (a resolved row offset).
    pub fn type_at(&self, idx: usize) -> Option<SqlType> {
        self.sides
            .iter()
            .find(|s| idx >= s.offset && idx < s.offset + s.columns.len())
            .map(|s| s.types[idx - s.offset])
    }

    /// Resolve with precise errors: ambiguous when 2+ sides match,
    /// unknown when 0 do. `resolve` (the eval fast path) collapses both
    /// to None -- callers that matter run [`check_expr`] first.
    pub fn resolve_checked(&self, table: Option<&str>, name: &str) -> SqlResult<usize> {
        let candidates: Vec<&ScopeSide> = self
            .sides
            .iter()
            .filter(|s| {
                let has_col = s.columns.iter().any(|c| c.eq_ignore_ascii_case(name));
                let matches_t = |t: &str| {
                    s.qualifier.eq_ignore_ascii_case(t) || s.table.eq_ignore_ascii_case(t)
                };
                has_col && table.is_none_or(matches_t)
            })
            .collect();
        match candidates.as_slice() {
            [] => Err(SqlError::new(
                ErrorCode::BadField,
                match table {
                    Some(t) => format!("unknown column '{t}.{name}'"),
                    None => format!("unknown column '{name}'"),
                },
            )),
            [side] => Ok(side.offset + col_pos(side, name)),
            _ => Err(SqlError::new(
                ErrorCode::BadField,
                format!("ambiguous column '{name}'"),
            )),
        }
    }
}

impl ColumnScope for FromScope {
    fn resolve(&self, table: Option<&str>, name: &str) -> Option<usize> {
        self.resolve_checked(table, name).ok()
    }
}

fn col_pos(side: &ScopeSide, name: &str) -> usize {
    side.columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case(name))
        .expect("candidate side holds the column")
}

/// Validate every column reference in `e` against `scope` so ambiguity
/// and unknown-column errors surface before evaluation starts.
pub fn check_expr(e: &Expr, scope: &FromScope) -> SqlResult<()> {
    match e {
        Expr::Col { table, name } => {
            scope.resolve_checked(table.as_deref(), name)?;
            Ok(())
        }
        Expr::Lit(_) | Expr::Placeholder => Ok(()),
        Expr::BinaryOp { left, right, .. } => {
            check_expr(left, scope)?;
            check_expr(right, scope)
        }
        Expr::Not(x) | Expr::Neg(x) => check_expr(x, scope),
        Expr::IsNull { expr, .. } => check_expr(expr, scope),
        Expr::InList { expr, list, .. } => {
            check_expr(expr, scope)?;
            for item in list {
                check_expr(item, scope)?;
            }
            Ok(())
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            check_expr(expr, scope)?;
            check_expr(low, scope)?;
            check_expr(high, scope)
        }
        Expr::Like { expr, pattern, .. } => {
            check_expr(expr, scope)?;
            check_expr(pattern, scope)
        }
        Expr::Agg { arg, .. } => match arg {
            Some(a) => check_expr(a, scope),
            None => Ok(()),
        },
        Expr::Func { args, .. } => {
            for a in args {
                check_expr(a, scope)?;
            }
            Ok(())
        }
    }
}

/// All live rows of `schema` visible at `read_ts`, ordered by primary
/// key (pk_key byte order -- deterministic scan output).
pub fn visible_rows(
    store: &Store,
    schema: &TableSchema,
    read_ts: u64,
) -> SqlResult<Vec<Vec<Value>>> {
    visible_rows_between(store, schema, read_ts, 0, u16::MAX)
}

/// Live rows whose slot lies in `[lo, hi]` (inclusive), visible at
/// `read_ts` and ordered by pk -- `visible_rows` restricted to one
/// slot band (the M3 scatter-gather unit: a node owns exactly its
/// band, so band scans are disjoint by construction).
pub fn visible_rows_between(
    store: &Store,
    schema: &TableSchema,
    read_ts: u64,
    lo: u16,
    hi: u16,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut rows = Vec::new();
    for (_pk, raw) in visible_versions_between(store, schema, read_ts, lo, hi)? {
        let (header, values) = row::decode_version(schema, &raw).map_err(SqlError::from)?;
        // Only live versions are collected below; re-checking keeps a
        // corrupt entry from resurrecting as a row.
        if header == HEADER_LIVE {
            rows.push(values);
        }
    }
    Ok(rows)
}

/// `(pk_key, raw LIVE version bytes)` of every row of `schema` in slot
/// band `[lo, hi]` visible at `read_ts`, ordered by pk bytes. This is
/// the shared core of the local scan and the dist `ScanBand` reply:
/// the raw bytes are exactly what `decode_version` consumes locally,
/// so a gathered row decodes identically to a locally scanned one.
pub fn visible_versions_between(
    store: &Store,
    schema: &TableSchema,
    read_ts: u64,
    lo: u16,
    hi: u16,
) -> SqlResult<Vec<(Vec<u8>, Vec<u8>)>> {
    // pk -> visible (ts, raw version). Versions of one pk arrive
    // newest-first (inverted ts suffix in the key), so the FIRST entry
    // with ts <= read_ts that is NOT a prepared (0x02) version is the
    // visible version (`or_insert` keeps it); prepared versions are
    // skipped so an in-flight 2PC write cannot shadow older commits.
    let mut newest: BTreeMap<Vec<u8>, (u64, Vec<u8>)> = BTreeMap::new();
    ops::for_each_from(store, b"0/", false, &mut |key, val| {
        if let Some((slot, table_id, pk, ts)) = row::parse_version_key(key) {
            // Slot keys are decimal strings, so byte order is NOT slot
            // order ("1000/" < "999/"): no prefix seek is possible, the
            // band is a numeric filter over the ordered walk.
            if lo <= slot
                && slot <= hi
                && table_id == schema.id
                && ts <= read_ts
                && !row::is_prepared(val)
            {
                newest.entry(pk).or_insert_with(|| (ts, val.to_vec()));
            }
        }
        true
    })
    .map_err(SqlError::from)?;
    let mut out = Vec::with_capacity(newest.len());
    for (pk, (_ts, raw)) in newest {
        // Tombstones stay invisible: the pk exists but has no live row.
        if raw.first() == Some(&HEADER_LIVE) {
            out.push((pk, raw));
        }
    }
    Ok(out)
}

/// Participant side of a remote `ScanBand`: resolve `table_id` in the
/// local (raft-replicated) catalog and hand back the raw visible
/// versions of that table's band. The catalog converges with the log,
/// so a table the coordinator can name is nameable here; an unknown id
/// (or any scan error) is deterministic and fails the caller's query.
pub fn band_rows(
    shared: &Shared,
    table_id: u32,
    read_ts: u64,
    lo: u16,
    hi: u16,
) -> SqlResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let schema = catalog::list_tables(shared)
        .into_iter()
        .find(|s| s.id == table_id)
        .ok_or_else(|| SqlError::new(ErrorCode::Unknown, format!("table id {table_id} unknown")))?;
    visible_versions_between(&shared.store, &schema, read_ts, lo, hi)
}

/// Live rows of exactly these pks (sorted, deduped) visible at
/// `read_ts` -- the IndexLookup row fetch. Stale index entries and
/// tombstoned/absent pks simply yield no row.
fn rows_by_pks(
    store: &Store,
    schema: &TableSchema,
    pks: &[Vec<u8>],
    read_ts: u64,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut rows = Vec::with_capacity(pks.len());
    for pk in pks {
        if let Some(values) =
            index::visible_row_at_pk(store, schema, pk, read_ts).map_err(SqlError::from)?
        {
            rows.push(values);
        }
    }
    Ok(rows)
}

/// Materialize a whole FROM clause at `read_ts`: single-table scan or
/// nested-loop join (cross join when ON is absent). An open txn's
/// staged writes are merged into every table side (own-write visibility).
///
/// A single-table FROM with a sargable WHERE conjunct on an indexed
/// column is read through the index instead (pks from the planner, one
/// seek per pk); the residual WHERE still applies afterwards, and the
/// txn overlay unions in staged rows the index cannot see.
pub fn materialize(
    shared: &Shared,
    tref: &TableRef,
    read_ts: u64,
    txn: Option<&crate::sql::tx::Txn>,
    filter: Option<&Expr>,
) -> SqlResult<Source> {
    match tref {
        TableRef::Table { name, alias } => {
            let schema = catalog::lookup(shared, name)
                .map_err(SqlError::from)?
                .ok_or_else(|| SqlError::no_such_table(name))?;
            let rows = match plan::plan(&shared.store, &schema, alias.as_deref(), filter) {
                plan::Path::IndexLookup { pks, .. } => {
                    rows_by_pks(&shared.store, &schema, &pks, read_ts)?
                }
                plan::Path::SeqScan => visible_rows(&shared.store, &schema, read_ts)?,
            };
            let rows = match txn {
                Some(t) => crate::sql::tx::merge_rows(&schema, rows, t)?,
                None => rows,
            };
            let mut scope = FromScope::default();
            scope.sides.push(table_side(&schema, alias));
            Ok(Source { scope, rows })
        }
        TableRef::Join { left, right, on } => {
            let l = materialize(shared, left, read_ts, txn, None)?;
            let r = materialize(shared, right, read_ts, txn, None)?;
            let left_width = l.scope.row_width();
            let mut scope = l.scope;
            for mut side in r.scope.sides {
                side.offset += left_width;
                scope.sides.push(side);
            }
            if let Some(cond) = on {
                check_expr(cond, &scope)?;
            }
            let mut rows = Vec::with_capacity(l.rows.len().saturating_mul(r.rows.len()));
            for lr in &l.rows {
                for rr in &r.rows {
                    let mut combined = lr.clone();
                    combined.extend_from_slice(rr);
                    if let Some(cond) = on {
                        // NULL/Unknown ON conditions drop the pair.
                        if !truthy(&eval(cond, &scope, &combined)?)? {
                            continue;
                        }
                    }
                    rows.push(combined);
                }
            }
            Ok(Source { scope, rows })
        }
    }
}

/// The FROM-scope side of one plain table reference.
pub fn table_side(schema: &TableSchema, alias: &Option<String>) -> ScopeSide {
    ScopeSide {
        qualifier: alias.clone().unwrap_or_else(|| schema.name.clone()),
        table: schema.name.clone(),
        columns: schema.columns.iter().map(|c| c.name.clone()).collect(),
        types: schema.columns.iter().map(|c| c.sql_type).collect(),
        offset: 0,
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
