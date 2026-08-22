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
use crate::sql::storage::schema::{TableSchema, Value};
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

/// One open snapshot transaction: pure state, no behavior attached.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Txn {
    /// Snapshot pin: reads see versions with `ts <= read_ts`.
    pub read_ts: u64,
    /// Staged writes; at most one entry per (table, pk).
    pub writes: BTreeMap<TxnKey, TxnWrite>,
}

/// BEGIN: pin the latest committed timestamp and register the snapshot
/// (keeps the GC watermark behind every open reader).
pub fn begin(oracle: &Oracle) -> Txn {
    let read_ts = oracle.now();
    oracle.register_snapshot(read_ts);
    Txn {
        read_ts,
        writes: BTreeMap::new(),
    }
}

/// Stage a full-width row (INSERT, or the new-pk half of a pk-moving
/// UPDATE). Later stages of the same pk replace earlier ones.
pub fn stage_upsert(txn: &mut Txn, schema: &TableSchema, values: Vec<Value>) -> SqlResult<()> {
    let pk = row::pk_encode(pk_value(schema, &values)).map_err(SqlError::from)?;
    txn.writes.insert((schema.id, pk), TxnWrite::Row(values));
    Ok(())
}

/// Stage a delete marker for one pk (DELETE, or the old-pk half of a
/// pk-moving UPDATE).
pub fn stage_delete(txn: &mut Txn, schema: &TableSchema, pk_key: Vec<u8>) {
    txn.writes.insert((schema.id, pk_key), TxnWrite::Tombstone);
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
    out
}

async fn commit_inner(shared: &Shared, txn: &Txn) -> SqlResult<()> {
    if txn.writes.is_empty() {
        return Ok(()); // read-only txn: nothing to validate or write
    }
    // M3: with a ready cluster and any remote slot-owner, the commit
    // becomes a 2PC (participants validate; the coordinator's own
    // slice runs through the same participant code by direct call).
    // Single-node deployments never enter this branch: the exact M2
    // batch sequence below stays untouched.
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
    let ts = shared.sql_ts.alloc_n(txn.writes.len() as u64);
    let mut batch = build_commit_batch(&txn.writes, &schemas, ts)?;
    crate::sql::index::maintain::apply_ops(&mut batch, idx);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)
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
#[path = "session_index_tests.rs"]
mod session_index_tests;
