//! FROM-clause materialization: table scans and nested-loop joins.
//!
//! A [`Source`] is a query's fully materialized input: the joined rows
//! (left side's columns first) plus the [`FromScope`] that resolves
//! column references into offsets of those rows. Scans are snapshot
//! reads: per primary key only the newest version with `ts <= read_ts`
//! is decoded, and tombstoned keys are invisible.

use std::collections::BTreeMap;

use crate::sql::exec::expr::{eval, truthy, ColumnScope};
use crate::sql::parse::ast::{Expr, TableRef};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
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
    // pk -> visible (ts, raw version). Versions of one pk arrive
    // newest-first (inverted ts suffix in the key), so the FIRST entry
    // with ts <= read_ts is the visible version (`or_insert` keeps it).
    let mut newest: BTreeMap<Vec<u8>, (u64, Vec<u8>)> = BTreeMap::new();
    ops::for_each_from(store, b"0/", false, &mut |key, val| {
        if let Some((_slot, table_id, pk, ts)) = row::parse_version_key(key) {
            if table_id == schema.id && ts <= read_ts {
                newest.entry(pk).or_insert_with(|| (ts, val.to_vec()));
            }
        }
        true
    })
    .map_err(SqlError::from)?;
    let mut rows = Vec::with_capacity(newest.len());
    for (_pk, (_ts, raw)) in newest {
        let (header, values) = row::decode_version(schema, &raw).map_err(SqlError::from)?;
        if header == HEADER_LIVE {
            rows.push(values);
        }
    }
    Ok(rows)
}

/// Materialize a whole FROM clause at `read_ts`: single-table scan or
/// nested-loop join (cross join when ON is absent).
pub fn materialize(shared: &Shared, tref: &TableRef, read_ts: u64) -> SqlResult<Source> {
    match tref {
        TableRef::Table { name, alias } => {
            let schema = catalog::lookup(shared, name)
                .map_err(SqlError::from)?
                .ok_or_else(|| SqlError::no_such_table(name))?;
            let rows = visible_rows(&shared.store, &schema, read_ts)?;
            let mut scope = FromScope::default();
            scope.sides.push(table_side(&schema, alias));
            Ok(Source { scope, rows })
        }
        TableRef::Join { left, right, on } => {
            let l = materialize(shared, left, read_ts)?;
            let r = materialize(shared, right, read_ts)?;
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
