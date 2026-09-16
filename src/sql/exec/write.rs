//! INSERT / UPDATE / DELETE: autocommitted single-batch writes, or --
//! from M2 on -- staged into the session's open transaction.
//!
//! Every statement first decides its writes PURELY (reading a snapshot
//! that merges the txn's own staged writes). Autocommit mode then stamps
//! the rows from one freshly allocated timestamp range and commits them
//! through the fsync write path; an open transaction instead overlays
//! the decisions onto its write buffer (`tx::stage_*`), flushed once at
//! COMMIT. Reads inside a transaction run at its pinned `read_ts`.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::dist;
use crate::sql::exec::expr::{coerce, eval, SingleTableScope};
use crate::sql::exec::scan::{self, FromScope};
use crate::sql::exec::select::{filter_rows, order_rows};
use crate::sql::exec::sequence;
use crate::sql::exec::write_probe;
use crate::sql::exec::{ExecOutcome, SqlSession};
use crate::sql::index::maintain::{self, Transition};
use crate::sql::index::{self, RowSide};
use crate::sql::parse::ast::{Expr, OrderKey, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::row;
use crate::sql::storage::schema::{KeyModel, TableSchema, Value};
use crate::sql::tx;
use crate::state::Shared;
use crate::store::ops;

pub async fn insert(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: Statement,
) -> SqlResult<ExecOutcome> {
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
    // AUTO_INCREMENT: the slot survives row building as NULL (the NOT
    // NULL check defers to the allocator); allocation then rewrites it
    // under the raft-replicated counter (see exec/sequence.rs).
    let ai = schema.auto_increment_index();
    let mut full_rows = Vec::with_capacity(rows.len());
    for exprs in &rows {
        full_rows.push(build_insert_row(&schema, &columns, exprs, ai)?);
    }
    if let Some(idx) = ai {
        let first_auto;
        (full_rows, first_auto) = sequence::allocate(shared, &schema.name, full_rows, idx).await?;
        if let Some(first) = first_auto {
            sess.last_insert_id = first;
            sequence::note_last_insert_id(first);
        }
    }
    let n = full_rows.len() as u64;
    if schema.engine.is_columnar() {
        return insert_columnar(shared, sess, &schema, full_rows).await;
    }
    if let Some(txn) = sess.txn.as_mut() {
        for values in full_rows {
            tx::stage_upsert(txn, &schema, values)?;
        }
        return Ok(ExecOutcome::Affected(n));
    }
    let pk_keys = full_rows
        .iter()
        .map(|r| pk_key_of(&schema, r))
        .collect::<SqlResult<Vec<_>>>()?;
    // StarRocks PRIMARY KEY model treats INSERT as UPSERT: recover each
    // pk's visible row BEFORE the batch is stamped so index maintenance
    // sees a replace (unique entries move with the new values) instead
    // of a blind insert that would leave stale entries behind. Other
    // models keep pure inserts (empty `old_rows`).
    let old_rows: Vec<Option<Vec<Value>>> = if schema.key_model == KeyModel::PrimaryKey {
        let now = shared.sql_ts.now();
        let mut olds = Vec::with_capacity(pk_keys.len());
        for pk in &pk_keys {
            olds.push(
                index::visible_row_at_pk(&shared.store, &schema, pk, now)
                    .map_err(SqlError::from)?,
            );
        }
        olds
    } else {
        Vec::new()
    };
    let trans: Vec<Transition<'_>> = full_rows
        .iter()
        .enumerate()
        .map(|(i, r)| Transition {
            old: old_rows.get(i).and_then(|o| o.as_ref()).map(|o| RowSide {
                pk_key: &pk_keys[i],
                values: o,
            }),
            new: Some(RowSide {
                pk_key: &pk_keys[i],
                values: r,
            }),
        })
        .collect();
    let idx = index_ops(shared, &schema, &trans)?;
    // Index maintenance runs BEFORE the rows land: a unique violation
    // must reject the statement without writing anything. M3: in a
    // ready cluster with any remote slot-owner the batch becomes a 2PC
    // (see sql::dist); single-node deployments keep the exact local
    // batch path below.
    let writes: dist::plan::SimpleWrites = full_rows
        .iter()
        .zip(pk_keys.iter())
        .map(|(r, pk)| (pk.clone(), Some(r)))
        .collect();
    let read_ts = shared.sql_ts.now();
    // Write frontier before planning (see write_probe): one ts per row.
    write_probe::reserve(
        shared,
        schema.id,
        read_ts,
        n,
        pk_keys.iter().map(|k| k.as_slice()),
        &idx,
    )
    .await?;
    if let Some(plan) = dist::plan::try_plan_simple(shared, read_ts, &schema, &writes, &idx)? {
        return dist::twopc::run(shared, &plan)
            .await
            .map(|_| ExecOutcome::Affected(n));
    }
    let ts = shared.sql_ts.alloc_n_above(n, read_ts);
    let mut batch = WriteBatch::default();
    for (i, values) in full_rows.iter().enumerate() {
        // Duplicate PKs inside one batch are legal: each row gets its
        // own increasing ts, so the LAST one wins for later readers.
        put_version(&mut batch, &schema, values, ts.start + i as u64)?;
    }
    maintain::apply_ops(&mut batch, idx);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Affected(n))
}

/// Columnar INSERT: append-only, no pk dedup, no index maintenance.
/// Autocommit flushes one LIVE segment per statement; an open txn
/// stages the rows into its append buffer (flushed once at COMMIT).
/// Per-statement and cumulative staged sizes are bounded by
/// `columnar_flush_rows` / `columnar_flush_bytes` (one segment per
/// flush by design, so an oversized batch cannot be split silently).
async fn insert_columnar(
    shared: &Shared,
    sess: &mut SqlSession,
    schema: &TableSchema,
    rows: Vec<Vec<Value>>,
) -> SqlResult<ExecOutcome> {
    use crate::sql::columnar::writer as cw;
    let row_limit = cw::flush_rows_limit(&shared.conf);
    let byte_limit = cw::flush_bytes_limit(&shared.conf);
    let stmt_bytes: u64 = rows.iter().map(|r| cw::estimate_row_bytes(r) as u64).sum();
    if rows.len() as u64 > row_limit {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "columnar INSERT of {} rows exceeds columnar_flush_rows ({row_limit}); \
                 split the statement",
                rows.len()
            ),
        ));
    }
    if stmt_bytes > byte_limit {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "columnar INSERT of {stmt_bytes} bytes exceeds columnar_flush_bytes \
                 ({byte_limit}); split the statement"
            ),
        ));
    }
    let n = rows.len() as u64;
    if let Some(txn) = sess.txn.as_mut() {
        // Cumulative per-table limits for the staged buffer: one
        // segment per table at COMMIT, so the buffer itself must stay
        // within the flush limits.
        let (staged_rows, staged_bytes) = txn
            .appends
            .get(&schema.name)
            .map(|v| {
                (
                    v.len() as u64,
                    v.iter().map(|r| cw::estimate_row_bytes(r) as u64).sum(),
                )
            })
            .unwrap_or((0, 0));
        if staged_rows + rows.len() as u64 > row_limit || staged_bytes + stmt_bytes > byte_limit {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!(
                    "columnar append buffer for '{}' would exceed columnar_flush_rows/\
                     columnar_flush_bytes",
                    schema.name
                ),
            ));
        }
        for values in rows {
            tx::stage_append(txn, &schema.name, values)?;
        }
        return Ok(ExecOutcome::Affected(n));
    }
    let commit_ts = shared.sql_ts.alloc();
    let segment_id = cw::local_segment_id(shared, schema.id, commit_ts)
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
    cw::commit_segment(shared, schema, segment_id, commit_ts, &rows).await?;
    Ok(ExecOutcome::Affected(n))
}

pub async fn update(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: Statement,
) -> SqlResult<ExecOutcome> {
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
    if schema.engine.is_columnar() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "columnar table '{table}' is append-only in this version: UPDATE is not supported"
            ),
        ));
    }
    let scope = single_table_scope(&schema);
    for (col, e) in &assignments {
        schema.column_index(col).ok_or_else(|| bad_field(col))?;
        scan::check_expr(e, &scope)?;
    }
    if let Some(f) = &filter {
        scan::check_expr(f, &scope)?;
    }
    let matched = matched_rows(
        shared,
        &schema,
        &scope,
        filter.as_ref(),
        &order_by,
        limit,
        sess.txn.as_ref(),
    )
    .await?;

    // Decide writes purely, then either stage them (txn) or stamp one
    // ts range over all versions (autocommit).
    struct Planned {
        tombstone_old_pk: Option<Vec<u8>>,
        old: Vec<Value>,
        values: Vec<Value>,
    }
    let mut plans: Vec<Planned> = Vec::new();
    for old in &matched {
        let new = apply_assignments(&schema, &scope, old, &assignments)?;
        if new == *old {
            continue; // unchanged rows write no version
        }
        // PK reassignment = tombstone the old pk + insert the new
        // (any pk column changing moves the row's physical key).
        let new_pk_key = pk_key_of(&schema, &new)?;
        let old_pk_key = pk_key_of(&schema, old)?;
        let tombstone_old_pk = (new_pk_key != old_pk_key).then_some(old_pk_key);
        plans.push(Planned {
            tombstone_old_pk,
            old: old.clone(),
            values: new,
        });
    }
    if plans.is_empty() {
        return Ok(ExecOutcome::Affected(0));
    }
    if let Some(txn) = sess.txn.as_mut() {
        let n = plans.len() as u64;
        for p in plans {
            if let Some(old_key) = p.tombstone_old_pk {
                tx::stage_delete(txn, &schema, old_key)?;
            }
            tx::stage_upsert(txn, &schema, p.values)?;
        }
        return Ok(ExecOutcome::Affected(n));
    }
    let versions = plans
        .iter()
        .map(|p| if p.tombstone_old_pk.is_some() { 2 } else { 1 })
        .sum::<u64>();
    // Index transitions BEFORE any row write: each plan carries its old
    // side (the matched row) so UPDATEs delete stale entries, and a
    // pk-moving update is one delete + one insert.
    let pk_sides = plans
        .iter()
        .map(|p| {
            let old_pk = match &p.tombstone_old_pk {
                Some(k) => k.clone(),
                None => pk_key_of(&schema, &p.old)?,
            };
            Ok((old_pk, pk_key_of(&schema, &p.values)?))
        })
        .collect::<SqlResult<Vec<(Vec<u8>, Vec<u8>)>>>()?;
    let trans: Vec<Transition<'_>> = plans
        .iter()
        .zip(pk_sides.iter())
        .flat_map(|(p, (old_pk, new_pk))| {
            let old_side = RowSide {
                pk_key: old_pk,
                values: &p.old,
            };
            let new_side = RowSide {
                pk_key: new_pk,
                values: &p.values,
            };
            if old_pk == new_pk {
                vec![Transition {
                    old: Some(old_side),
                    new: Some(new_side),
                }]
            } else {
                vec![
                    Transition {
                        old: Some(old_side),
                        new: None,
                    },
                    Transition {
                        old: None,
                        new: Some(new_side),
                    },
                ]
            }
        })
        .collect();
    let idx = index_ops(shared, &schema, &trans)?;
    // M3 2PC hook (see the INSERT path note): the write list widens a
    // pk-moving plan into tombstone + row, exactly the two versions
    // the local batch below stamps.
    let mut dist_writes: dist::plan::SimpleWrites = Vec::with_capacity(plans.len());
    for p in &plans {
        if let Some(old_key) = &p.tombstone_old_pk {
            dist_writes.push((old_key.clone(), None));
        }
        dist_writes.push((pk_key_of(&schema, &p.values)?, Some(&p.values)));
    }
    let read_ts = shared.sql_ts.now();
    // Write frontier before planning (see write_probe): one ts per
    // write, pk moves add a tombstone.
    write_probe::reserve(
        shared,
        schema.id,
        read_ts,
        dist_writes.len() as u64,
        dist_writes.iter().map(|(pk, _)| pk.as_slice()),
        &idx,
    )
    .await?;
    if let Some(plan) = dist::plan::try_plan_simple(shared, read_ts, &schema, &dist_writes, &idx)? {
        return dist::twopc::run(shared, &plan)
            .await
            .map(|_| ExecOutcome::Affected(plans.len() as u64));
    }
    let ts = shared.sql_ts.alloc_n_above(versions, read_ts);
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
    maintain::apply_ops(&mut batch, idx);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Affected(plans.len() as u64))
}

pub async fn delete(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: Statement,
) -> SqlResult<ExecOutcome> {
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
    if schema.engine.is_columnar() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "columnar table '{table}' is append-only in this version: DELETE is not supported"
            ),
        ));
    }
    let scope = single_table_scope(&schema);
    if let Some(f) = &filter {
        scan::check_expr(f, &scope)?;
    }
    let matched = matched_rows(
        shared,
        &schema,
        &scope,
        filter.as_ref(),
        &order_by,
        limit,
        sess.txn.as_ref(),
    )
    .await?;
    let n = matched.len() as u64;
    if n == 0 {
        return Ok(ExecOutcome::Affected(0));
    }
    if let Some(txn) = sess.txn.as_mut() {
        for r in matched {
            tx::stage_delete(txn, &schema, pk_key_of(&schema, &r)?)?;
        }
        return Ok(ExecOutcome::Affected(n));
    }
    // Index entries of every deleted row leave in the same batch;
    // unique checks are trivially satisfied (deletes claim nothing).
    let pk_keys = matched
        .iter()
        .map(|r| pk_key_of(&schema, r))
        .collect::<SqlResult<Vec<_>>>()?;
    let trans: Vec<Transition<'_>> = matched
        .iter()
        .zip(pk_keys.iter())
        .map(|(r, pk)| {
            Transition::delete(RowSide {
                pk_key: pk,
                values: r,
            })
        })
        .collect();
    let idx = index_ops(shared, &schema, &trans)?;
    // M3 2PC hook (see the INSERT path note).
    let writes: dist::plan::SimpleWrites = pk_keys.iter().map(|pk| (pk.clone(), None)).collect();
    let read_ts = shared.sql_ts.now();
    // Write frontier before planning (see write_probe): one ts per row.
    write_probe::reserve(
        shared,
        schema.id,
        read_ts,
        writes.len() as u64,
        pk_keys.iter().map(|k| k.as_slice()),
        &idx,
    )
    .await?;
    if let Some(plan) = dist::plan::try_plan_simple(shared, read_ts, &schema, &writes, &idx)? {
        return dist::twopc::run(shared, &plan)
            .await
            .map(|_| ExecOutcome::Affected(n));
    }
    let ts = shared.sql_ts.alloc_n_above(n, read_ts);
    let mut batch = WriteBatch::default();
    for (i, r) in matched.iter().enumerate() {
        let key = pk_key_of(&schema, r)?;
        let slot = row::row_slot(&schema, &key);
        batch.put(
            row::version_key(&schema, slot, &key, ts.start + i as u64),
            row::encode_tombstone(),
        );
    }
    maintain::apply_ops(&mut batch, idx);
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

/// WHERE + ORDER BY + LIMIT shared by UPDATE and DELETE. In a ready
/// multi-node cluster the candidate rows are gathered from every slot
/// owner exactly like a SELECT (a local scan would see only this
/// node's band and silently miss remote rows). Inside a txn the read
/// runs at its pinned `read_ts` MERGED with its staged writes, so
/// UPDATE-twice/DELETE-then-UPDATE chains see own writes.
async fn matched_rows(
    shared: &Shared,
    schema: &TableSchema,
    scope: &FromScope,
    filter: Option<&Expr>,
    order_by: &[OrderKey],
    limit: Option<u64>,
    txn: Option<&crate::sql::tx::Txn>,
) -> SqlResult<Vec<Vec<Value>>> {
    // Autocommit matching takes its read point fresh: fold the raft
    // cursor frontier first so `now()` rides the cluster's latest
    // applied ts (a follower coordinating an UPDATE would otherwise
    // match at its stale ts-block tail and silently miss rows stamped
    // above it). Txn mode keeps the snapshot pinned at BEGIN.
    let read_ts = txn.map(|t| t.read_ts).unwrap_or_else(|| {
        shared.sql_ts.sync_cursor_frontier();
        shared.sql_ts.now()
    });
    // Same fan-out verdict as a SELECT: per-owner bands when the
    // cluster is ready (UPDATE/DELETE reject columnar tables before
    // this point, so the band gather is the only distributed shape).
    let mut rows = match dist::gather::gatherable_by_name(shared) {
        Some(bs) => dist::gather::gather_rows(shared, &bs, schema, read_ts).await?,
        None => scan::visible_rows(&shared.store, schema, read_ts)?,
    };
    if let Some(t) = txn {
        rows = crate::sql::tx::merge_rows(schema, rows, t)?;
    }
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
/// column types and enforce NOT NULL. `ai` is the AUTO_INCREMENT slot:
/// its NULL check is deferred (the allocator assigns an id right after
/// and never leaves NULL behind).
fn build_insert_row(
    schema: &TableSchema,
    columns: &[String],
    exprs: &[Expr],
    ai: Option<usize>,
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
        if Some(i) != ai {
            check_not_null(&v, &col.name, col.nullable)?;
        }
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
        Expr::Subquery(_) | Expr::InSubquery { .. } => Err(SqlError::new(
            ErrorCode::NotSupported,
            "subqueries are not allowed in VALUES",
        )),
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

/// Encoded primary key of a full-width row (all pk columns, in pk
/// order; one component for single-column pks).
fn pk_key_of(schema: &TableSchema, values: &[Value]) -> SqlResult<Vec<u8>> {
    row::pk_encode_row(schema, values).map_err(SqlError::from)
}

/// Index-entry ops of one autocommit batch (no-op for indexless tables):
/// unique constraints are validated BEFORE the caller writes any row,
/// and the returned ops go into the SAME RocksDB batch as the rows.
fn index_ops(
    shared: &Shared,
    schema: &TableSchema,
    transitions: &[Transition<'_>],
) -> SqlResult<crate::sql::index::IndexOps> {
    maintain::batch_ops(&shared.store, schema, transitions)
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
