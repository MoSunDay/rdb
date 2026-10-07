//! INSERT / UPDATE / DELETE: autocommitted single-batch writes, or --
//! from M2 on -- staged into the session's open transaction.
//!
//! Every statement first decides its writes PURELY (reading a snapshot
//! that merges the txn's own staged writes). Autocommit mode then stamps
//! the rows from one freshly allocated timestamp range and commits them
//! through the fsync write path; an open transaction instead overlays
//! the decisions onto its write buffer (`tx::stage_*`), flushed once at
//! COMMIT. Reads inside a transaction run at its pinned `read_ts`.
//!
//! The INSERT family (plain / ODKU / REPLACE / INSERT ... SELECT) is
//! decided here and in `exec::upsert` + `exec::insert_select`; every
//! decided statement converges on [`apply_writes`], the one row-batch
//! sink (versions + index ops + write-frontier probe + 2PC-or-local
//! commit).

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::dist;
use crate::sql::exec::expr::{coerce, eval};
use crate::sql::exec::insert_common::{bad_field, build_row_values, check_not_null, eval_cells};
use crate::sql::exec::scan::{self, FromScope};
use crate::sql::exec::select::{filter_rows, order_rows};
use crate::sql::exec::sequence;
use crate::sql::exec::upsert;
use crate::sql::exec::write_probe;
use crate::sql::exec::{ExecOutcome, SqlSession};
use crate::sql::index::maintain::{self, Transition};
use crate::sql::index::{self, RowSide};
use crate::sql::parse::ast::{ConflictAction, Expr, InsertSource, OrderKey, Statement};
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
        source,
        conflict,
    } = stmt
    else {
        unreachable!("dispatch maps only Insert here");
    };
    let schema = lookup(shared, &table)?;
    if schema.engine.is_columnar() {
        // The explicit conflict paths need the row engine (unique-index
        // reads/migration), and INSERT ... SELECT needs the pre-write
        // materialization semantics; the columnar engine is append-only
        // (ER 1235, same gating family as UPDATE/DELETE below).
        if !matches!(conflict, ConflictAction::Error) || matches!(source, InsertSource::Select(_)) {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!(
                    "columnar table '{table}' is append-only in this version: \
                     {kind} is not supported",
                    kind = conflict_label(&conflict, &source)
                ),
            ));
        }
    }
    // Cluster veto (plan `m2-dml-conflicts` decision point 1, choice
    // (b)): ODKU/REPLACE need the CONFLICTING EXISTING ROW at decide
    // time, and the conflict read runs on the coordinator's local
    // snapshot -- in a multi-node cluster the conflicting row (or its
    // unique-index entry) usually lives on another slot owner, so the
    // decision would silently miss it. A correct cluster conflict read
    // is a per-key gather ahead of the 2PC (>= 1 RPC per probed pk and
    // unique value), over the plan's "<= 2-RPC point read" bar for
    // choice (a). Fails fast here (the forwarding boundary) with
    // ER 1235; standalone / single-band clusters keep full support and
    // e2e asserts this message.
    if !matches!(conflict, ConflictAction::Error) && cluster_spans_remote(shared) {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "{} is not supported in cluster mode (m2-dml-conflicts decision 1b)",
                conflict_label(&conflict, &source)
            ),
        ));
    }
    // Row sources converge on evaluated cell values first: VALUES
    // tuples evaluate here, a SELECT source materializes to completion
    // BEFORE any write (pre-statement snapshot for same-table reads).
    let cell_rows: Vec<Vec<Value>> = match &source {
        InsertSource::Values(rows) => {
            let mut out = Vec::with_capacity(rows.len());
            for exprs in rows {
                out.push(eval_cells(&schema, exprs)?);
            }
            out
        }
        InsertSource::Select(cq) => {
            crate::sql::exec::insert_select::materialize(shared, sess, cq, &columns, &schema)
                .await?
        }
    };
    if cell_rows.is_empty() {
        return Ok(ExecOutcome::Affected(0));
    }
    // AUTO_INCREMENT: the slot survives row building as NULL (the NOT
    // NULL check defers to the allocator); allocation then rewrites it
    // under the raft-replicated counter (see exec/sequence.rs). Ids are
    // allocated for EVERY incoming row before conflict decisions, so an
    // ODKU row that takes the update branch burns its reserved id --
    // same gap semantics as MySQL's ODKU.
    let ai = schema.auto_increment_index();
    let mut full_rows = Vec::with_capacity(cell_rows.len());
    for cells in cell_rows {
        full_rows.push(build_row_values(&schema, &columns, cells, ai)?);
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
    match conflict {
        ConflictAction::OnDuplicate(assigns) => {
            upsert::run_odku(shared, sess, &schema, full_rows, &assigns).await
        }
        ConflictAction::Replace => upsert::run_replace(shared, sess, &schema, full_rows).await,
        ConflictAction::Error => plain_insert(shared, sess, &schema, full_rows, n).await,
    }
}

/// Human label of an INSERT flavor for reject messages.
fn conflict_label(conflict: &ConflictAction, source: &InsertSource) -> &'static str {
    match (conflict, source) {
        (ConflictAction::Replace, _) => "REPLACE INTO",
        (ConflictAction::OnDuplicate(_), _) => "ON DUPLICATE KEY UPDATE",
        (ConflictAction::Error, InsertSource::Select(_)) => "INSERT ... SELECT",
        (ConflictAction::Error, InsertSource::Values(_)) => "INSERT",
    }
}

/// True when a ready cluster has any participant besides this node:
/// the exact condition under which a write becomes a 2PC.
fn cluster_spans_remote(shared: &Shared) -> bool {
    dist::routing(shared).is_some_and(|r| r.addrs.iter().any(|a| a != &r.host))
}

/// Plain INSERT (no explicit conflict clause): the pre-M2 behavior is
/// kept verbatim -- pk collisions take the silent last-writer-wins
/// upsert (StarRocks PRIMARY KEY model recovers the old row first so
/// index maintenance sees a replace), unique-index collisions reject
/// 1062 inside `apply_writes`' unique validation.
async fn plain_insert(
    shared: &Shared,
    sess: &mut SqlSession,
    schema: &TableSchema,
    full_rows: Vec<Vec<Value>>,
    n: u64,
) -> SqlResult<ExecOutcome> {
    if let Some(txn) = sess.txn.as_mut() {
        for values in full_rows {
            tx::stage_upsert(txn, schema, values)?;
        }
        return Ok(ExecOutcome::Affected(n));
    }
    let read_ts = shared.sql_ts.now();
    let mut writes: Vec<RowWrite> = Vec::with_capacity(full_rows.len());
    for values in full_rows {
        let pk = pk_key_of(schema, &values)?;
        // StarRocks PRIMARY KEY model treats INSERT as UPSERT: recover
        // each pk's visible row BEFORE the batch is stamped so index
        // maintenance sees a replace (unique entries move with the new
        // values) instead of a blind insert leaving stale entries.
        let old = if schema.key_model == KeyModel::PrimaryKey {
            index::visible_row_at_pk(&shared.store, schema, &pk, read_ts).map_err(SqlError::from)?
        } else {
            None
        };
        writes.push(RowWrite {
            deletes: old.map(|o| vec![(pk.clone(), o)]).unwrap_or_default(),
            put: Some(values),
        });
    }
    apply_writes(shared, sess, schema, writes, read_ts, n).await
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

    // Decide writes purely, then either stage them (txn) or hand the
    // decided rows to the shared batch sink.
    let mut writes: Vec<RowWrite> = Vec::new();
    let mut any_moved = false;
    for old in &matched {
        let new = apply_assignments(&schema, &scope, old, &assignments)?;
        if new == *old {
            continue; // unchanged rows write no version
        }
        // PK reassignment = tombstone the old pk + insert the new
        // (any pk column changing moves the row's physical key); the
        // old side always rides along for index maintenance. The sink
        // derives the tombstone from `deletes[0].pk != put pk`.
        let (opk, npk) = (pk_key_of(&schema, old)?, pk_key_of(&schema, &new)?);
        if npk != opk {
            any_moved = true;
        }
        writes.push(RowWrite {
            deletes: vec![(opk, old.clone())],
            put: Some(new),
        });
    }
    if writes.is_empty() {
        return Ok(ExecOutcome::Affected(0));
    }
    // A moved pk may not land on a row OTHER than the one being moved
    // (MySQL ER 1062). The probe set is the live view FOLDED with
    // each decided write in statement order: a target vacated earlier
    // in the same statement is free (MySQL processes rows in retrieval
    // order), while two rows converging on one target fail loudly on
    // the second with ER 1062; cross-statement txn collisions are
    // covered because `visible_live_rows` merges the txn's staged
    // writes. Gated on "some write moved a pk" so normal UPDATEs pay
    // no extra visibility scan.
    if any_moved {
        let live = visible_live_rows(shared, &schema, sess.txn.as_ref()).await?;
        let mut pks = live
            .iter()
            .map(|r| pk_key_of(&schema, r))
            .collect::<SqlResult<std::collections::BTreeSet<Vec<u8>>>>()?;
        for w in &writes {
            let Some(put) = &w.put else { continue };
            let opk = &w.deletes[0].0;
            let npk = pk_key_of(&schema, put)?;
            if npk != *opk && pks.contains(&npk) {
                return Err(pk_dup_entry(&schema, put));
            }
            pks.remove(opk);
            pks.insert(npk);
        }
    }
    let affected = writes.len() as u64;
    apply_writes(shared, sess, &schema, writes, shared.sql_ts.now(), affected).await
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
    let mut writes: Vec<RowWrite> = Vec::with_capacity(matched.len());
    for r in matched {
        writes.push(RowWrite {
            deletes: vec![(pk_key_of(&schema, &r)?, r)],
            put: None,
        });
    }
    apply_writes(shared, sess, &schema, writes, shared.sql_ts.now(), n).await
}

fn lookup(shared: &Shared, table: &str) -> SqlResult<TableSchema> {
    catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))
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
    let mut rows = visible_live_rows(shared, schema, txn).await?;
    rows = filter_rows(&rows, scope, filter)?;
    if !order_by.is_empty() {
        order_rows(&mut rows, order_by, scope)?;
    }
    if let Some(l) = limit {
        rows.truncate(l as usize);
    }
    Ok(rows)
}

/// The UNFILTERED live row set behind [`matched_rows`]: identical
/// visibility (autocommit: frontier-synced `now()`; txn: pinned
/// `read_ts` merged with staged writes; cluster: per-owner band
/// gather). The pk-move check below needs it because a WHERE clause
/// may have skipped the very row a moved pk wants to land on.
async fn visible_live_rows(
    shared: &Shared,
    schema: &TableSchema,
    txn: Option<&crate::sql::tx::Txn>,
) -> SqlResult<Vec<Vec<Value>>> {
    // Autocommit takes its read point fresh: fold the raft cursor
    // frontier first so `now()` rides the cluster's latest applied ts
    // (a follower coordinating a write would otherwise match at its
    // stale ts-block tail and silently miss rows stamped above it).
    // Txn mode keeps the snapshot pinned at BEGIN.
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
    Ok(rows)
}

/// Apply SET assignments: every expression evaluates against the
/// CURRENT row (all of them, then applied), coerced to the column type;
/// NOT NULL is re-checked on the result.
pub(crate) fn apply_assignments(
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
pub(crate) fn pk_key_of(schema: &TableSchema, values: &[Value]) -> SqlResult<Vec<u8>> {
    row::pk_encode_row(schema, values).map_err(SqlError::from)
}

/// ER 1062 for a pk move landing on an occupied pk, in the engine's
/// duplicate-entry shape `Duplicate entry <pk shown> for key
/// 'PRIMARY'` (composite pks render their column values joined with
/// '-'; see `index::dup_entry`).
pub(crate) fn pk_dup_entry(schema: &TableSchema, row: &[Value]) -> SqlError {
    let pk = schema.pk_indices();
    if pk.len() == 1 {
        return index::dup_entry(&row[pk[0]], "PRIMARY");
    }
    let shown = pk
        .iter()
        .map(|&i| index::keys::value_display(&row[i]))
        .collect::<Vec<_>>()
        .join("-");
    SqlError::new(
        ErrorCode::DupEntry,
        format!("Duplicate entry '{shown}' for key 'PRIMARY'"),
    )
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
pub(crate) fn single_table_scope(schema: &TableSchema) -> FromScope {
    FromScope {
        sides: vec![scan::table_side(schema, &None)],
    }
}

/// One decided row write of a DML batch, the unit every write path
/// (plain INSERT / UPDATE / DELETE / ODKU / REPLACE) converges on:
/// the old rows leaving the table (physical pk + full values, feeding
/// index maintenance and -- when their pk differs from the put's --
/// tombstones) and optionally the new full-width row landing.
pub(crate) struct RowWrite {
    pub deletes: Vec<(Vec<u8>, Vec<Value>)>,
    pub put: Option<Vec<Value>>,
}

/// Normalized [`RowWrite`]: put pk precomputed, tombstone verdict per
/// delete (a delete whose pk equals the put pk is an in-place replace
/// and stamps no tombstone).
struct Sides {
    dels: Vec<(Vec<u8>, Vec<Value>, bool)>,
    put: Option<(Vec<u8>, Vec<Value>)>,
}

fn sides_of(schema: &TableSchema, writes: &[RowWrite]) -> SqlResult<Vec<Sides>> {
    writes
        .iter()
        .map(|w| {
            let put = match &w.put {
                Some(v) => Some((pk_key_of(schema, v)?, v.clone())),
                None => None,
            };
            Ok(Sides {
                dels: w
                    .deletes
                    .iter()
                    .map(|(pk, vals)| {
                        let tombstone = put.as_ref().map(|(ppk, _)| ppk != pk).unwrap_or(true);
                        (pk.clone(), vals.clone(), tombstone)
                    })
                    .collect(),
                put,
            })
        })
        .collect()
}

/// Apply one statement's decided row writes: an open txn stages them
/// into its write buffer; autocommit derives the index-entry ops
/// (unique constraints validated BEFORE anything lands), reserves the
/// write frontier, then either 2PCs to the slot owners (any remote
/// participant) or stamps one local batch. `affected` is the caller's
/// statement-level affected-rows count (INSERT/UPDATE/DELETE/ODKU/REPLACE
/// each count differently); every physical version consumes one ts of
/// the allocated range, in batch order.
pub(crate) async fn apply_writes(
    shared: &Shared,
    sess: &mut SqlSession,
    schema: &TableSchema,
    writes: Vec<RowWrite>,
    read_ts: u64,
    affected: u64,
) -> SqlResult<ExecOutcome> {
    if let Some(txn) = sess.txn.as_mut() {
        for w in &writes {
            for (pk, _) in &w.deletes {
                tx::stage_delete(txn, schema, pk.clone())?;
            }
            if let Some(values) = &w.put {
                tx::stage_upsert(txn, schema, values.clone())?;
            }
        }
        return Ok(ExecOutcome::Affected(affected));
    }
    let sides = sides_of(schema, &writes)?;
    let versions = sides
        .iter()
        .map(|s| s.dels.iter().filter(|(_, _, t)| *t).count() as u64 + u64::from(s.put.is_some()))
        .sum::<u64>();
    // Index transitions BEFORE any row write: each delete carries its
    // old row side (stale entries leave), each put its new side, and a
    // delete whose pk equals the put pk folds into ONE replace
    // transition so unique entries move instead of flickering.
    let trans: Vec<Transition<'_>> = sides
        .iter()
        .flat_map(|s| {
            let mut ts: Vec<Transition<'_>> = Vec::with_capacity(s.dels.len() + 1);
            for (pk, vals, tombstone) in &s.dels {
                if !*tombstone {
                    // in-place replace: old+new in one transition
                    if let Some((ppk, pvals)) = &s.put {
                        ts.push(Transition {
                            old: Some(RowSide {
                                pk_key: pk,
                                values: vals,
                            }),
                            new: Some(RowSide {
                                pk_key: ppk,
                                values: pvals,
                            }),
                        });
                    }
                } else {
                    ts.push(Transition::delete(RowSide {
                        pk_key: pk,
                        values: vals,
                    }));
                }
            }
            if let Some((ppk, pvals)) = &s.put {
                // a put whose pk matched no delete is a pure insert
                if s.dels.iter().all(|(dpk, _, _)| dpk != ppk) {
                    ts.push(Transition::insert(RowSide {
                        pk_key: ppk,
                        values: pvals,
                    }));
                }
            }
            ts
        })
        .collect();
    let idx = index_ops(shared, schema, &trans)?;
    // M3 2PC hook: the write list widens pk moves into tombstone + row,
    // exactly the versions the local batch below stamps.
    let mut dist_writes: dist::plan::SimpleWrites = Vec::with_capacity(sides.len());
    for s in &sides {
        for (pk, _, tombstone) in &s.dels {
            if *tombstone {
                dist_writes.push((pk.clone(), None));
            }
        }
        if let Some((ppk, pvals)) = &s.put {
            dist_writes.push((ppk.clone(), Some(pvals)));
        }
    }
    // Write frontier before planning (see write_probe): one ts per version.
    write_probe::reserve(
        shared,
        schema.id,
        read_ts,
        versions,
        dist_writes.iter().map(|(pk, _)| pk.as_slice()),
        &idx,
    )
    .await?;
    if let Some(plan) = dist::plan::try_plan_simple(shared, read_ts, schema, &dist_writes, &idx)? {
        return dist::twopc::run(shared, &plan)
            .await
            .map(|_| ExecOutcome::Affected(affected));
    }
    let ts = shared.sql_ts.alloc_n_above(versions, read_ts);
    let mut batch = WriteBatch::default();
    let mut next = ts.start;
    for s in &sides {
        for (pk, _, tombstone) in &s.dels {
            if *tombstone {
                let slot = row::row_slot(schema, pk);
                batch.put(
                    row::version_key(schema, slot, pk, next),
                    row::encode_tombstone(),
                );
                next += 1;
            }
        }
        if let Some((_, vals)) = &s.put {
            put_version(&mut batch, schema, vals, next)?;
            next += 1;
        }
    }
    maintain::apply_ops(&mut batch, idx);
    // Same-batch ts floor (restart clock fencing, see tx::floor).
    tx::floor::stamp(&mut batch, ts.end - 1);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Affected(affected))
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
