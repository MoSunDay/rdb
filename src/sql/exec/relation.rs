//! Materialized in-memory relations: CTE bodies, derived tables, and
//! set-operation operands. A [`Relation`] is pure data (column
//! metadata + rows); the [`CteScope`] carries CTEs visible to a query
//! being executed, keyed case-insensitively like catalog tables.

use std::sync::Arc;

use crate::sql::exec::scan::{FromScope, ScopeSide, Source};
use crate::sql::exec::ColMeta;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;

/// One materialized result set: columns plus rows.
#[derive(Debug, Clone)]
pub struct Relation {
    pub columns: Vec<ColMeta>,
    pub rows: Arc<Vec<Vec<Value>>>,
}

impl Relation {
    pub fn new(columns: Vec<ColMeta>, rows: Vec<Vec<Value>>) -> Self {
        Relation {
            columns,
            rows: Arc::new(rows),
        }
    }

    /// Resolution scope over this relation's columns (single side,
    /// qualified by the FROM alias / CTE name).
    pub fn scope(&self, qualifier: &str) -> FromScope {
        FromScope {
            sides: vec![ScopeSide {
                qualifier: qualifier.to_string(),
                table: qualifier.to_string(),
                columns: self.columns.iter().map(|c| c.name.clone()).collect(),
                types: self.columns.iter().map(|c| c.sql_type).collect(),
                // A derived relation has no declared nullability or key.
                nullable: vec![true; self.columns.len()],
                key_pos: None,
                offset: 0,
            }],
        }
    }

    /// Wrap this relation as a FROM [`Source`] (rows copied out of
    /// the shared Arc so downstream joins/filters own plain vectors).
    pub fn into_source(self, alias: &str) -> Source {
        Source {
            scope: self.scope(alias),
            rows: Arc::try_unwrap(self.rows).unwrap_or_else(|arc| (*arc).clone()),
        }
    }

    /// Apply a positional column-alias list (`WITH x (a, b) AS ...`):
    /// renames output columns in place; arity must match.
    pub fn rename_columns(&mut self, aliases: &[String]) -> SqlResult<()> {
        if aliases.is_empty() {
            return Ok(());
        }
        if aliases.len() != self.columns.len() {
            return Err(SqlError::new(
                ErrorCode::BadField,
                format!(
                    "CTE column list has {} names but the query yields {} columns",
                    aliases.len(),
                    self.columns.len()
                ),
            ));
        }
        for (col, name) in self.columns.iter_mut().zip(aliases) {
            col.name = name.clone();
        }
        Ok(())
    }
}

/// CTEs visible to the query under execution. Later CTEs may shadow
/// earlier same-named ones (lookup scans in reverse).
#[derive(Debug, Clone, Default)]
pub struct CteScope {
    entries: Vec<(String, Relation)>,
}

impl CteScope {
    /// Case-insensitive lookup (last definition wins, like MySQL's
    /// duplicate-CTE name resolution).
    pub fn lookup(&self, name: &str) -> Option<&Relation> {
        self.entries
            .iter()
            .rev()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, rel)| rel)
    }

    pub fn push(&mut self, name: String, rel: Relation) {
        self.entries.push((name, rel));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Build a Relation from a select-path result pair.
pub fn relation_of(columns: Vec<ColMeta>, rows: Vec<Vec<Value>>) -> Relation {
    Relation::new(columns, rows)
}
