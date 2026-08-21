//! INSERT / UPDATE / DELETE: autocommitted single-batch writes.
//!
//! Every statement is one MVCC write batch stamped from one freshly
//! allocated timestamp range: rows are read at the oracle's `now()`
//! snapshot, the new versions (+tombstones) are decided purely first,
//! then committed through the fsync write path. There is no
//! cross-statement transaction state in M1.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::exec::expr::{coerce, eval, SingleTableScope};
use crate::sql::exec::scan::{self, FromScope};
use crate::sql::exec::select::{filter_rows, order_rows};
use crate::sql::exec::ExecOutcome;
use crate::sql::parse::ast::{Expr, OrderKey, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::row;
use crate::sql::storage::schema::{TableSchema, Value};
use crate::state::Shared;
use crate::store::ops;

pub async fn insert(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    let Statement::Insert {
        table,
        columns,
        rows,
    } = stmt
    else {
        unreachable!("dispatch maps only Insert here");
    };
    let schema = lookup(shared, &table)?;
    if rows.is_empty() {
        return Ok(ExecOutcome::Affected(0));
    }
    let mut full_rows = Vec::with_capacity(rows.len());
    for exprs in &rows {
        full_rows.push(build_insert_row(&schema, &columns, exprs)?);
    }
    let n = full_rows.len() as u64;
    let ts = shared.sql_ts.alloc_n(n);
    let mut batch = WriteBatch::default();
    for (i, values) in full_rows.iter().enumerate() {
        // Duplicate PKs inside one batch are legal: each row gets its
        // own increasing ts, so the LAST one wins for later readers.
        put_version(&mut batch, &schema, values, ts.start + i as u64)?;
    }
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Affected(n))
}

pub async fn update(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    let Statement::Update {
        table,
        assignments,
        filter,
        order_by,
        limit,
    } = stmt
    else {
        unreachable!("dispatch maps only Update here");
    };
    let schema = lookup(shared, &table)?;
    let scope = single_table_scope(&schema);
    for (col, e) in &assignments {
        schema.column_index(col).ok_or_else(|| bad_field(col))?;
        scan::check_expr(e, &scope)?;
    }
    if let Some(f) = &filter {
        scan::check_expr(f, &scope)?;
    }
    let matched = matched_rows(shared, &schema, &scope, filter.as_ref(), &order_by, limit)?;

    // Decide writes purely, then stamp one ts range over all versions.
    struct Planned {
        tombstone_old_pk: Option<Vec<u8>>,
        values: Vec<Value>,
    }
    let mut plans: Vec<Planned> = Vec::new();
    for old in &matched {
        let new = apply_assignments(&schema, &scope, old, &assignments)?;
        if new == *old {
            continue; // unchanged rows write no version
        }
        // PK reassignment = tombstone the old pk + insert the new.
        let tombstone_old_pk = (new[schema.pk_index()] != old[schema.pk_index()])
            .then(|| pk_key_of(&schema, old))
            .transpose()?;
        plans.push(Planned {
            tombstone_old_pk,
            values: new,
        });
    }
    let versions = plans
        .iter()
        .map(|p| if p.tombstone_old_pk.is_some() { 2 } else { 1 })
        .sum::<u64>();
    if versions == 0 {
        return Ok(ExecOutcome::Affected(0));
    }
    let ts = shared.sql_ts.alloc_n(versions);
    let mut batch = WriteBatch::default();
    let mut next = ts.start;
    for p in &plans {
        if let Some(old_key) = &p.tombstone_old_pk {
            let slot = row::row_slot(&schema, old_key);
            batch.put(
                row::version_key(&schema, slot, old_key, next),
                row::encode_tombstone(),
            );
            next += 1;
        }
        put_version(&mut batch, &schema, &p.values, next)?;
        next += 1;
    }
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Affected(plans.len() as u64))
}

pub async fn delete(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    let Statement::Delete {
        table,
        filter,
        order_by,
        limit,
    } = stmt
    else {
        unreachable!("dispatch maps only Delete here");
    };
    let schema = lookup(shared, &table)?;
    let scope = single_table_scope(&schema);
    if let Some(f) = &filter {
        scan::check_expr(f, &scope)?;
    }
    let matched = matched_rows(shared, &schema, &scope, filter.as_ref(), &order_by, limit)?;
    let n = matched.len() as u64;
    if n == 0 {
        return Ok(ExecOutcome::Affected(0));
    }
    let ts = shared.sql_ts.alloc_n(n);
    let mut batch = WriteBatch::default();
    for (i, r) in matched.iter().enumerate() {
        let key = pk_key_of(&schema, r)?;
        let slot = row::row_slot(&schema, &key);
        batch.put(
            row::version_key(&schema, slot, &key, ts.start + i as u64),
            row::encode_tombstone(),
        );
    }
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Affected(n))
}

fn lookup(shared: &Shared, table: &str) -> SqlResult<TableSchema> {
    catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))
}

fn bad_field(col: &str) -> SqlError {
    SqlError::new(
        ErrorCode::BadField,
        format!("unknown column '{col}' in 'field list'"),
    )
}

/// WHERE + ORDER BY + LIMIT shared by UPDATE and DELETE.
fn matched_rows(
    shared: &Shared,
    schema: &TableSchema,
    scope: &FromScope,
    filter: Option<&Expr>,
    order_by: &[OrderKey],
    limit: Option<u64>,
) -> SqlResult<Vec<Vec<Value>>> {
    let read_ts = shared.sql_ts.now();
    let mut rows = scan::visible_rows(&shared.store, schema, read_ts)?;
    rows = filter_rows(&rows, scope, filter)?;
    if !order_by.is_empty() {
        order_rows(&mut rows, order_by, scope)?;
    }
    if let Some(l) = limit {
        rows.truncate(l as usize);
    }
    Ok(rows)
}

/// Build one full-width row from an INSERT VALUES tuple: expand the
/// given columns (missing columns become NULL), evaluate the value
/// expressions (no row context -- column refs rejected), coerce to the
/// column types and enforce NOT NULL.
fn build_insert_row(
    schema: &TableSchema,
    columns: &[String],
    exprs: &[Expr],
) -> SqlResult<Vec<Value>> {
    for e in exprs {
        reject_col_refs(e)?;
    }
    let names: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    let scope = SingleTableScope { columns: &names };
    let mut slots: Vec<Option<Value>> = vec![None; schema.columns.len()];
    if columns.is_empty() {
        // Positional form: the tuple must name every column in order.
        if exprs.len() != schema.columns.len() {
            return Err(SqlError::new(
                ErrorCode::WrongValueCount,
                format!(
                    "row has {} values, table '{}' has {} columns",
                    exprs.len(),
                    schema.name,
                    schema.columns.len()
                ),
            ));
        }
        for (i, e) in exprs.iter().enumerate() {
            slots[i] = Some(eval(e, &scope, &[])?);
        }
    } else {
        if exprs.len() != columns.len() {
            return Err(SqlError::new(
                ErrorCode::WrongValueCount,
                "column count doesn't match value count",
            ));
        }
        for (col, e) in columns.iter().zip(exprs) {
            let idx = schema.column_index(col).ok_or_else(|| bad_field(col))?;
            if slots[idx].is_some() {
                return Err(SqlError::new(
                    ErrorCode::Parse,
                    format!("column '{col}' specified twice"),
                ));
            }
            slots[idx] = Some(eval(e, &scope, &[])?);
        }
    }
    let mut out = Vec::with_capacity(schema.columns.len());
    for (i, col) in schema.columns.iter().enumerate() {
        let v = coerce(slots[i].take().unwrap_or(Value::Null), col.sql_type)?;
        check_not_null(&v, &col.name, col.nullable)?;
        out.push(v);
    }
    Ok(out)
}

fn check_not_null(v: &Value, name: &str, nullable: bool) -> SqlResult<()> {
    if matches!(v, Value::Null) && !nullable {
        return Err(SqlError::new(
            ErrorCode::BadNull,
            format!("column '{name}' cannot be null"),
        ));
    }
    Ok(())
}

/// Column references are not allowed in VALUES (no row exists yet).
fn reject_col_refs(e: &Expr) -> SqlResult<()> {
    match e {
        Expr::Col { name, .. } => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("column '{name}' is not allowed in VALUES"),
        )),
        Expr::Lit(_) | Expr::Placeholder | Expr::Agg { arg: None, .. } => Ok(()),
        Expr::Agg { arg: Some(a), .. } => reject_col_refs(a),
        Expr::Func { args, .. } => {
            for a in args {
                reject_col_refs(a)?;
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            reject_col_refs(left)?;
            reject_col_refs(right)
        }
        Expr::Not(x) | Expr::Neg(x) => reject_col_refs(x),
        Expr::IsNull { expr, .. } => reject_col_refs(expr),
        Expr::InList { expr, list, .. } => {
            reject_col_refs(expr)?;
            for i in list {
                reject_col_refs(i)?;
            }
            Ok(())
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            reject_col_refs(expr)?;
            reject_col_refs(low)?;
            reject_col_refs(high)
        }
        Expr::Like { expr, pattern, .. } => {
            reject_col_refs(expr)?;
            reject_col_refs(pattern)
        }
    }
}

/// Apply SET assignments: every expression evaluates against the
/// CURRENT row (all of them, then applied), coerced to the column type;
/// NOT NULL is re-checked on the result.
fn apply_assignments(
    schema: &TableSchema,
    scope: &FromScope,
    old: &[Value],
    assignments: &[(String, Expr)],
) -> SqlResult<Vec<Value>> {
    let mut sets: Vec<(usize, Value)> = Vec::with_capacity(assignments.len());
    for (col, e) in assignments {
        let idx = schema.column_index(col).ok_or_else(|| bad_field(col))?;
        let v = coerce(eval(e, scope, old)?, schema.columns[idx].sql_type)?;
        sets.push((idx, v));
    }
    let mut new = old.to_vec();
    for (idx, v) in sets {
        new[idx] = v;
    }
    for (i, col) in schema.columns.iter().enumerate() {
        check_not_null(&new[i], &col.name, col.nullable)?;
    }
    Ok(new)
}

/// Encoded primary key of a full-width row.
fn pk_key_of(schema: &TableSchema, values: &[Value]) -> SqlResult<Vec<u8>> {
    row::pk_encode(&values[schema.pk_index()]).map_err(SqlError::from)
}

/// Write one live row version at `ts`.
fn put_version(
    batch: &mut WriteBatch,
    schema: &TableSchema,
    values: &[Value],
    ts: u64,
) -> SqlResult<()> {
    let key = pk_key_of(schema, values)?;
    let slot = row::row_slot(schema, &key);
    let encoded = row::encode_row(schema, values).map_err(SqlError::from)?;
    batch.put(row::version_key(schema, slot, &key, ts), encoded);
    Ok(())
}

/// Single-table FROM scope used by UPDATE/DELETE (and assignment eval).
fn single_table_scope(schema: &TableSchema) -> FromScope {
    FromScope {
        sides: vec![scan::table_side(schema, &None)],
    }
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
