//! Rudimentary access-path planner (M2): pick IndexLookup vs SeqScan for
//! a SELECT over a single table.
//!
//! The planner splits the WHERE into top-level AND conjuncts and looks
//! for the first sargable predicate -- `col = lit`, `col IN (lits...)`,
//! `col BETWEEN lit AND lit` -- whose column carries an index (a
//! single-column index on exactly that column, from the raft catalog).
//! It then RESOLVES the matching pks through the index (the executor
//! still re-applies the full WHERE afterwards: defense in depth against
//! stale entries and planner drift).
//!
//! Heuristics: fall back to SeqScan when no conjunct is usable, when the
//! resolved pk count exceeds [`MAX_INDEX_PKS`] (a selective index stops
//! paying for itself), or when the FROM is a join (nested loop, M2).
//! Everything here is read-only; errors degrade to SeqScan, never fail
//! the query.

use crate::sql::exec::expr::coerce;
use crate::sql::index::{self, IndexRef};
use crate::sql::parse::ast::{BinOp, Expr};
use crate::sql::storage::schema::{SqlType, TableSchema, Value};
use crate::store::Store;

/// Index lookups stop being profitable past this many resolved pks.
pub const MAX_INDEX_PKS: usize = 1000;

/// Chosen access path for one table.
#[derive(Debug, Clone, PartialEq)]
pub enum Path {
    SeqScan,
    /// Fetch exactly these pks (sorted, deduped) instead of a full walk.
    IndexLookup {
        index: IndexMeta,
        pks: Vec<Vec<u8>>,
    },
}

/// The catalog bits of one index the planner chose.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    pub name: String,
    pub column: String,
    pub col_pos: u32,
    pub unique: bool,
}

/// One sargable conjunct, literals already coerced to the column type.
#[derive(Debug, Clone, PartialEq)]
enum Pred {
    Eq(Value),
    In(Vec<Value>),
    Between(Value, Value),
}

/// Plan the access path of a single-table SELECT. `alias` is the FROM
/// alias (qualified refs may use it or the bare table name).
pub fn plan(
    store: &Store,
    schema: &TableSchema,
    alias: Option<&str>,
    filter: Option<&Expr>,
) -> Path {
    let Some(filter) = filter else {
        return Path::SeqScan;
    };
    let mut conjuncts = Vec::new();
    collect_conjuncts(filter, &mut conjuncts);
    for c in conjuncts {
        let Some((col_name, pred)) = sargable(c, schema, alias) else {
            continue;
        };
        let Some(def) = schema.index_of_column(&col_name) else {
            continue; // no index on exactly this column
        };
        let index = IndexRef::of(def);
        let col_pos = schema.column_index(&col_name).unwrap_or(usize::MAX);
        if col_pos == usize::MAX {
            continue;
        }
        let pks = resolve(store, schema, &index, &pred);
        let pks = match pks {
            Some(p) if p.len() <= MAX_INDEX_PKS => p,
            _ => return Path::SeqScan, // too wide or lookup failed
        };
        return Path::IndexLookup {
            index: IndexMeta {
                name: def.name.clone(),
                column: def.column.clone(),
                col_pos: col_pos as u32,
                unique: def.unique,
            },
            pks,
        };
    }
    Path::SeqScan
}

/// Split `a AND b AND c` into [a, b, c] (AND binds tighter than OR, so
/// an OR at the top level stays one opaque conjunct -> SeqScan).
fn collect_conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::BinaryOp {
        left,
        op: BinOp::And,
        right,
    } = e
    {
        collect_conjuncts(left, out);
        collect_conjuncts(right, out);
    } else {
        out.push(e);
    }
}

/// Recognize `col = lit` (either side), `col IN (lit, ...)`,
/// `col BETWEEN lit AND lit` -- not negated, literals only. The
/// literals are coerced to the column type; failures yield None (the
/// residual filter reports the real error later).
fn sargable(e: &Expr, schema: &TableSchema, alias: Option<&str>) -> Option<(String, Pred)> {
    let ty = |name: &str| -> Option<SqlType> {
        Some(schema.columns[schema.column_index(name)?].sql_type)
    };

    let lit = |x: &Expr| match x {
        Expr::Lit(v) => Some(v.clone()),
        _ => None,
    };
    match e {
        Expr::BinaryOp {
            left,
            op: BinOp::Eq,
            right,
        } => {
            let (col, l) = match (column_of(left, schema, alias), lit(right)) {
                (Some(c), Some(l)) => (c, l),
                _ => {
                    // reversed literal = column
                    let l = lit(left)?;
                    let c = column_of(right, schema, alias)?;
                    (c, l)
                }
            };
            Some((col.clone(), Pred::Eq(coerce(l, ty(col)?).ok()?)))
        }
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            let col = column_of(expr, schema, alias)?;
            let t = ty(col)?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                let v = lit(item)?;
                vals.push(coerce(v, t).ok()?);
            }
            Some((col.clone(), Pred::In(vals)))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            let col = column_of(expr, schema, alias)?;
            let t = ty(col)?;
            let lo = coerce(lit(low)?, t).ok()?;
            let hi = coerce(lit(high)?, t).ok()?;
            Some((col.clone(), Pred::Between(lo, hi)))
        }
        _ => None,
    }
}

/// The column name of a bare/qualified reference into THIS table
/// (qualifier must match the table name or its alias, case-insensitive).
fn column_of<'a>(e: &'a Expr, schema: &TableSchema, alias: Option<&str>) -> Option<&'a String> {
    let Expr::Col { table, name } = e else {
        return None;
    };
    if let Some(q) = table {
        let ok = q.eq_ignore_ascii_case(&schema.name)
            || alias.is_some_and(|a| q.eq_ignore_ascii_case(a));
        if !ok {
            return None;
        }
    }
    schema.column_index(name).map(|_| name)
}

/// Resolve the pks a predicate selects, through the index entries.
/// Lookup failures degrade to None (-> SeqScan). Output is deduped and
/// sorted, matching the executor's pk-order output contract.
fn resolve(
    store: &Store,
    schema: &TableSchema,
    index: &IndexRef,
    pred: &Pred,
) -> Option<Vec<Vec<u8>>> {
    let raw = match pred {
        Pred::Eq(v) => index::lookup_pks(store, schema, index, v).ok()?,
        Pred::In(vals) => {
            let mut out = Vec::new();
            for v in vals {
                if matches!(v, Value::Null) {
                    continue; // NULL never matches
                }
                out.extend(index::lookup_pks(store, schema, index, v).ok()?);
            }
            out
        }
        Pred::Between(lo, hi) => index::lookup_range(store, schema, index, lo, hi).ok()?,
    };
    let deduped: std::collections::BTreeSet<Vec<u8>> = raw.into_iter().collect();
    Some(deduped.into_iter().collect())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
