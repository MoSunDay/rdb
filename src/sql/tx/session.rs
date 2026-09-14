//! Explicit BEGIN/COMMIT/ROLLBACK snapshot transactions (M2).
//!
//! A [`Txn`] is plain data: the pinned read timestamp plus the staged
//! write set (`table_id + pk_key -> row | tombstone`). Every operation
//! on it is a free function -- staging overlays the buffer (last write
//! per pk wins), snapshot reads MERGE the buffer over the store's
//! visible rows, and COMMIT validates then flushes one deterministic
//! MVCC batch:
//!
//! 1. `conflict_check`: first-committer-wins -- for every staged pk the
//!    newest committed version must be at `ts <= read_ts`, else the
//!    commit fails with error 1213 and the client retries;
//! 2. `alloc_n(len)`: one timestamp per staged write, assigned in
//!    `(table_id, pk_key)` byte order so the batch is reproducible;
//! 3. `build_commit_batch`: one version (row or tombstone) per write;
//! 4. the batch goes through the fsync write path and the snapshot is
//!    unregistered on EVERY exit path (success, conflict, io error).
//!
//! ROLLBACK and a dropped connection just unregister + drop the buffer;
//! nothing staged ever touched the store.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::row;
use crate::sql::storage::schema::{Engine, TableSchema, Value};
use crate::sql::tx::Oracle;
use crate::state::Shared;
use crate::store::ops;
use crate::store::Store;

/// Buffered write identity: `(table_id, encoded primary key)`.
pub type TxnKey = (u32, Vec<u8>);

/// One buffered write: a full-width row or a delete marker.
#[derive(Debug, Clone, PartialEq)]
pub enum TxnWrite {
    Row(Vec<Value>),
    Tombstone,
}

/// One `SAVEPOINT name` marker. It snapshots the staged write set in
/// full (a later stage can OVERWRITE a pre-marker pk in place, so a
/// bare length truncation could not restore the pre-marker state),
/// records the append-buffer lengths (appends are append-only, so
/// lengths restore exactly), and the latch keys held at creation --
/// `ROLLBACK TO` keeps those and releases latches taken after it.
#[derive(Debug, Clone, PartialEq)]
pub struct Savepoint {
    pub name: String,
    pub writes: BTreeMap<TxnKey, TxnWrite>,
    /// table name -> staged append count at marker time.
    pub appends: BTreeMap<String, usize>,
    /// Latch keys this txn held when the marker was set.
    pub latches: Vec<crate::sql::tx::latch::LatchKey>,
}

/// One open snapshot transaction: pure state, no behavior attached.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Txn {
    /// Snapshot pin: reads see versions with `ts <= read_ts`.
    pub read_ts: u64,
    /// Unique id of this txn (latch-registry owner). `Txn::default()`
    /// keeps 0 and never takes latches; real BEGINs allocate above it.
    pub id: u64,
    /// Staged writes; at most one entry per (table, pk).
    pub writes: BTreeMap<TxnKey, TxnWrite>,
    /// Columnar appends per table name, staged at INSERT, flushed at COMMIT.
    pub appends: BTreeMap<String, Vec<Vec<Value>>>,
    /// Engine of the first staged write; later stages of the other
    /// engine are rejected (one transaction touches one engine).
    pub mode: Option<Engine>,
    /// SAVEPOINT markers in creation order; a repeated name shadows
    /// (lookups find the LAST marker of that name).
    pub savepoints: Vec<Savepoint>,
}

/// BEGIN: pin the latest committed timestamp and register the snapshot
/// (keeps the GC watermark behind every open reader).
pub fn begin(oracle: &Oracle) -> Txn {
    let read_ts = oracle.now();
    oracle.register_snapshot(read_ts);
    Txn {
        read_ts,
        id: crate::sql::tx::latch::next_owner_id(),
        writes: BTreeMap::new(),
        appends: BTreeMap::new(),
        mode: None,
        savepoints: Vec::new(),
    }
}

/// Stage one full-width row into the columnar append buffer of its
/// table (INSERT on a columnar table; append-only, no pk dedup).
/// Rejects a transaction that already staged row-store writes: one
/// transaction touches exactly one engine.
pub fn stage_append(txn: &mut Txn, table_name: &str, values: Vec<Value>) -> SqlResult<()> {
    check_engine(txn, Engine::Columnar)?;
    txn.mode = Some(Engine::Columnar);
    txn.appends
        .entry(table_name.to_string())
        .or_default()
        .push(values);
    Ok(())
}

/// Stage a full-width row (INSERT, or the new-pk half of a pk-moving
/// UPDATE). Later stages of the same pk replace earlier ones. Rejects
/// a transaction that already staged columnar appends.
pub fn stage_upsert(txn: &mut Txn, schema: &TableSchema, values: Vec<Value>) -> SqlResult<()> {
    check_engine(txn, Engine::Row)?;
    txn.mode = Some(Engine::Row);
    let pk = row::pk_encode(pk_value(schema, &values)).map_err(SqlError::from)?;
    txn.writes.insert((schema.id, pk), TxnWrite::Row(values));
    Ok(())
}

/// Stage a delete marker for one pk (DELETE, or the old-pk half of a
/// pk-moving UPDATE). Rejects a transaction that already staged
/// columnar appends.
pub fn stage_delete(txn: &mut Txn, schema: &TableSchema, pk_key: Vec<u8>) -> SqlResult<()> {
    check_engine(txn, Engine::Row)?;
    txn.mode = Some(Engine::Row);
    txn.writes.insert((schema.id, pk_key), TxnWrite::Tombstone);
    Ok(())
}

// ---- SAVEPOINT / ROLLBACK TO / RELEASE SAVEPOINT (MySQL semantics) ----

/// MySQL 1305: `SAVEPOINT {name} does not exist`.
pub fn unknown_savepoint(name: &str) -> SqlError {
    SqlError::new(
        ErrorCode::UnknownSavepoint,
        format!("SAVEPOINT {name} does not exist"),
    )
}

/// `SAVEPOINT name`: record a marker over the current staged state.
/// Reusing an existing name is legal and SHADOWS the old marker (the
/// most recent marker of a name wins; the old one stays in the stack
/// but is unreachable by name until the newer one is dropped).
pub fn savepoint(txn: &mut Txn, name: &str) {
    txn.savepoints.push(Savepoint {
        name: name.to_string(),
        writes: txn.writes.clone(),
        appends: txn
            .appends
            .iter()
            .map(|(t, rows)| (t.clone(), rows.len()))
            .collect(),
        latches: crate::sql::tx::latch::held_by(txn.id),
    });
}

/// Position of the NEWEST marker named `name` (savepoint names are
/// case-insensitive, like MySQL identifiers).
fn marker_of(txn: &Txn, name: &str) -> Option<usize> {
    txn.savepoints
        .iter()
        .rposition(|s| s.name.eq_ignore_ascii_case(name))
}

/// `ROLLBACK TO [SAVEPOINT] name`: restore the staged write set and
/// append buffers to the marker's snapshot. The txn itself stays open,
/// the marker itself is KEPT, and every savepoint created after it is
/// dropped (MySQL semantics). Row latches taken BEFORE the savepoint
/// are kept (MySQL keeps locks acquired before the savepoint); ones
/// taken after it are released with the undone writes.
pub fn rollback_to(txn: &mut Txn, name: &str) -> SqlResult<()> {
    let pos = marker_of(txn, name).ok_or_else(|| unknown_savepoint(name))?;
    let marker = txn.savepoints[pos].clone();
    txn.writes = marker.writes.clone();
    // appends are append-only: truncating each table's buffer to its
    // marker-time length restores the exact staged prefix; tables the
    // marker never saw disappear entirely.
    let lens = marker.appends.clone();
    txn.appends.retain(|t, _| lens.contains_key(t));
    for (t, rows) in txn.appends.iter_mut() {
        let len = lens.get(t).copied().unwrap_or(0);
        rows.truncate(len);
    }
    txn.savepoints.truncate(pos + 1);
    let keep: std::collections::BTreeSet<_> = marker.latches.into_iter().collect();
    let stale: Vec<_> = crate::sql::tx::latch::held_by(txn.id)
        .into_iter()
        .filter(|k| !keep.contains(k))
        .collect();
    crate::sql::tx::latch::release_keys(txn.id, &stale);
    Ok(())
}

/// `RELEASE [SAVEPOINT] name`: forget the marker and every savepoint
/// created after it. Nothing staged is undone (RELEASE is bookkeeping
/// only, per MySQL).
pub fn release_savepoint(txn: &mut Txn, name: &str) -> SqlResult<()> {
    let pos = marker_of(txn, name).ok_or_else(|| unknown_savepoint(name))?;
    txn.savepoints.truncate(pos);
    Ok(())
}

/// Enforce the one-engine-per-transaction rule at the single staging
/// choke point every write path funnels through.
fn check_engine(txn: &Txn, engine: Engine) -> SqlResult<()> {
    match txn.mode {
        None => Ok(()),
        Some(m) if m == engine => Ok(()),
        Some(m) => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!(
                "a transaction cannot mix row-store and columnar writes                  (already staged a {} write)",
                if m.is_columnar() { "columnar" } else { "row-store" }
            ),
        )),
    }
}

/// Own-write visibility: overlay the staged writes of `schema.id` on
/// the store's snapshot rows. Tombstones drop rows, staged rows replace
/// matching store rows and INJECT pks the store scan did not produce
/// (fresh inserts, or keys whose visible version is a tombstone).
/// Output stays ordered by pk_key bytes, like the store scan.
pub fn merge_rows(
    schema: &TableSchema,
    store_rows: Vec<Vec<Value>>,
    txn: &Txn,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut merged: BTreeMap<Vec<u8>, Vec<Value>> = BTreeMap::new();
    for r in store_rows {
        let pk = row::pk_encode(pk_value(schema, &r)).map_err(SqlError::from)?;
        merged.insert(pk, r);
    }
    for ((table_id, pk), w) in &txn.writes {
        if *table_id != schema.id {
            continue;
        }
        match w {
            TxnWrite::Row(values) => {
                merged.insert(pk.clone(), values.clone());
            }
            TxnWrite::Tombstone => {
                merged.remove(pk);
            }
        }
    }
    Ok(merged.into_values().collect())
}

/// First-committer-wins validation: every staged pk must have its
/// newest committed version at `ts <= read_ts`. A version committed
/// after our snapshot means someone else won the race -> 1213.
pub fn conflict_check(store: &Store, txn: &Txn) -> SqlResult<()> {
    for (table_id, pk) in txn.writes.keys() {
        if let Some(ts) = newest_version_ts(store, *table_id, pk).map_err(SqlError::from)? {
            if ts > txn.read_ts {
                return Err(SqlError::new(
                    ErrorCode::WriteConflict,
                    format!(
                        "write-write conflict on PK (table {table_id}, key {}): \
                         committed at ts {ts} after snapshot ts {}",
                        hex(pk),
                        txn.read_ts
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Newest committed version ts of (table_id, pk_key), if any. The
/// versions of one pk sort newest-first, so the FIRST store key at or
/// after the version prefix is the newest one -- one seek per staged pk.
/// Prepared (0x02) versions count too: an in-flight 2PC write must
/// lose a racing prepare exactly like a committed one.
pub(crate) fn newest_version_ts(
    store: &Store,
    table_id: u32,
    pk_key: &[u8],
) -> Result<Option<u64>, String> {
    let prefix = row::version_prefix(table_id, pk_key);
    let mut found: Option<u64> = None;
    ops::for_each_from(store, &prefix, false, &mut |key, _| {
        if key.starts_with(&prefix) {
            if let Some((_, _, _, ts)) = row::parse_version_key(key) {
                found = Some(ts);
            }
        }
        false // first matching key IS the newest; stop either way
    })?;
    Ok(found)
}

/// Schemas of every table the write set touches, by table id.
pub fn written_schemas(shared: &Shared, txn: &Txn) -> BTreeMap<u32, TableSchema> {
    let ids: Vec<u32> = txn.writes.keys().map(|(id, _)| *id).collect();
    catalog::list_tables(shared)
        .into_iter()
        .filter(|s| ids.contains(&s.id))
        .map(|s| (s.id, s))
        .collect()
}

/// The commit MVCC batch: staged writes in deterministic
/// `(table_id, pk_key)` order, one timestamp each from `ts_range`
/// (consumed sequentially). Exactly one version per staged write --
/// intra-txn overwrite already collapsed in the buffer.
pub fn build_commit_batch(
    writes: &BTreeMap<TxnKey, TxnWrite>,
    schemas: &BTreeMap<u32, TableSchema>,
    ts_range: Range<u64>,
) -> SqlResult<WriteBatch> {
    let mut batch = WriteBatch::default();
    for (i, ((table_id, pk), w)) in writes.iter().enumerate() {
        let next = ts_range.start + i as u64;
        let schema = schemas.get(table_id).ok_or_else(|| {
            SqlError::new(
                ErrorCode::NoSuchTable,
                format!("table {table_id} was dropped during the transaction"),
            )
        })?;
        let key = row::version_key(schema, row::slot_of(*table_id, pk), pk, next);
        let val = match w {
            TxnWrite::Row(values) => row::encode_row(schema, values).map_err(SqlError::from)?,
            TxnWrite::Tombstone => row::encode_tombstone(),
        };
        batch.put(key, val);
    }
    Ok(batch)
}

/// COMMIT: validate -> stamp -> flush. The snapshot is released on every
/// exit path; a failed commit leaves nothing behind (the caller dropped
/// the buffer by handing it over).
pub async fn commit(shared: &Shared, txn: Txn) -> SqlResult<()> {
    let out = commit_inner(shared, &txn).await;
    shared.sql_ts.unregister_snapshot(txn.read_ts);
    // Every exit path releases the txn's row latches (success,
    // conflict, io error): a failed commit must not strand locks.
    crate::sql::tx::latch::release_owner(txn.id);
    out
}

async fn commit_inner(shared: &Shared, txn: &Txn) -> SqlResult<()> {
    if txn.writes.is_empty() && txn.appends.is_empty() {
        return Ok(()); // read-only txn: nothing to validate or write
    }
    // M3: with a ready cluster and any remote slot-owner, the commit
    // becomes a 2PC (participants validate; the coordinator's own
    // slice runs through the same participant code by direct call).
    // Single-node deployments never enter this branch: the exact M2
    // batch sequence below stays untouched. Segments travel inside the
    // plan attached to the coordinator's own slice.
    // The write frontier is reserved BEFORE planning so the plan's ts
    // range can never stamp versions below the txn's read point; the
    // plan (or the local batch below) allocates one ts per staged
    // write plus one per appended segment.
    let want = txn.writes.len() as u64 + txn.appends.len() as u64;
    // Strict when any row write's slot owner is remote (the commit will
    // run as 2PC): an unreachable ts authority then vetoes the commit
    // with a retryable error instead of stamping GAP-fallback versions.
    // Appends always ride the coordinator's own slice, so only row
    // writes decide (the same keys `dist::plan::build` routes).
    let probes: Vec<Vec<u8>> = txn
        .writes
        .keys()
        .map(|(table_id, pk)| crate::sql::dist::row_probe(*table_id, pk))
        .collect();
    let strict = crate::sql::dist::any_remote_owner(shared, &probes);
    shared
        .sql_ts
        .reserve_write_frontier(txn.read_ts, want, strict)
        .await?;
    if let Some(plan) = crate::sql::dist::plan::try_plan_txn(shared, txn)? {
        return crate::sql::dist::twopc::run(shared, &plan).await;
    }
    conflict_check(&shared.store, txn)?;
    let schemas = written_schemas(shared, txn);
    // Index maintenance BEFORE the rows land: old row sides are
    // recovered from the store at the txn's own snapshot (the same
    // versions its reads would have seen), unique claims are validated
    // against them, and the entry ops ride in the same commit batch.
    let idx = commit_index_ops(shared, txn, &schemas)?;
    // Rows and columnar appends share ONE ts range and publish in one
    // atomic batch: rows take the head, one ts per columnar table's
    // segment takes the tail (BTreeMap order = deterministic).
    let total = txn.writes.len() as u64 + txn.appends.len() as u64;
    let ts = shared.sql_ts.alloc_n_above(total, txn.read_ts);
    let row_ts = ts.start..ts.start + txn.writes.len() as u64;
    let mut batch = build_commit_batch(&txn.writes, &schemas, row_ts)?;
    crate::sql::index::maintain::apply_ops(&mut batch, idx);
    let mut metas = Vec::new();
    for (i, (table, rows)) in txn.appends.iter().enumerate() {
        let commit_ts = ts.start + txn.writes.len() as u64 + i as u64;
        let meta = crate::sql::columnar::commit::flush_appends(shared, table, rows, commit_ts)?;
        let encoded = crate::sql::columnar::meta::encode_meta(&meta)
            .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
        batch.put(
            crate::sql::columnar::meta::meta_key(meta.table_id, meta.segment_id),
            encoded,
        );
        metas.push(meta);
    }
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    for m in &metas {
        crate::sql::columnar::registry_of(shared).insert(m);
    }
    Ok(())
}

/// Index-entry ops of a whole staged write set, per table, with unique
/// constraints checked at the txn snapshot. Rows the txn inserts (no
/// visible old version) plan as inserts; staged rows replace their
/// snapshot-visible predecessors; tombstones plan as deletes. Unique
/// claims therefore see exactly what a serial re-execution would.
pub fn commit_index_ops(
    shared: &Shared,
    txn: &Txn,
    schemas: &BTreeMap<u32, TableSchema>,
) -> SqlResult<crate::sql::index::IndexOps> {
    use crate::sql::index::{maintain, RowSide};

    let mut ops = crate::sql::index::IndexOps::new();
    for (table_id, schema) in schemas {
        // old row sides recovered at the snapshot (owned here, borrowed
        // by the transitions below)
        let mut olds: Vec<(Vec<u8>, Option<Vec<Value>>)> = Vec::new();
        let mut news: Vec<(&Vec<u8>, Option<&Vec<Value>>)> = Vec::new();
        for ((tid, pk), w) in &txn.writes {
            if tid != table_id {
                continue;
            }
            let old = crate::sql::index::visible_row_at_pk(&shared.store, schema, pk, txn.read_ts)
                .map_err(SqlError::from)?;
            olds.push((pk.clone(), old));
            news.push((
                pk,
                match w {
                    crate::sql::tx::TxnWrite::Row(values) => Some(values),
                    crate::sql::tx::TxnWrite::Tombstone => None,
                },
            ));
        }
        let transitions: Vec<maintain::Transition<'_>> = olds
            .iter()
            .zip(news.iter())
            .map(|((pk, old), (npk, new))| maintain::Transition {
                old: old.as_ref().map(|o| RowSide {
                    pk_key: pk,
                    values: o,
                }),
                new: new.map(|n| RowSide {
                    pk_key: npk,
                    values: n,
                }),
            })
            .collect();
        ops.extend(maintain::batch_ops(&shared.store, schema, &transitions)?);
    }
    Ok(ops)
}

/// ROLLBACK (and connection-end cleanup): release the snapshot and
/// discard the staged writes -- none of them ever reached the store.
pub fn rollback(oracle: &Oracle, txn: Txn) {
    oracle.unregister_snapshot(txn.read_ts);
    // Savepoints and row latches die with the txn.
    crate::sql::tx::latch::release_owner(txn.id);
}

fn pk_value<'a>(schema: &TableSchema, values: &'a [Value]) -> &'a Value {
    &values[schema.pk_index()]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

#[cfg(test)]
#[path = "savepoint_tests.rs"]
mod savepoint_tests;

#[cfg(test)]
#[path = "session_index_tests.rs"]
mod session_index_tests;
