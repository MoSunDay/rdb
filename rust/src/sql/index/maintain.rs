//! Batch-level index maintenance shared by BOTH write paths.
//!
//! Autocommit statements (exec::write) and transaction COMMIT
//! (tx::session) describe their row transitions as a list of
//! [`Transition`]s and call [`batch_ops`]: one pure derivation over
//! old-vs-new row values produces every index put/delete, plus the
//! unique-constraint validation that must run BEFORE the row batch is
//! written.
//!
//! Unique checking (per unique index):
//! 1. intra-batch: two live rows claiming the same value with different
//!    pks fail immediately (multi-row INSERT, self-joins of values);
//! 2. against the store: a put whose value is owned on disk by a
//!    DIFFERENT pk fails -- unless the same batch deletes that entry
//!    first (the owner row is being updated/deleted by this batch, e.g.
//!    a value swap between two rows of one UPDATE).
//!
//! M2 race window (documented, accepted): the disk check and the batch
//! write are not atomic across connections, so two concurrent writers
//! can both pass the check for the same value; the second batch then
//! overwrites the unique entry (last writer wins). A stricter scheme
//! needs write intents or raft-serialized writes (M3).

use std::collections::{BTreeMap, BTreeSet};

use rocksdb::WriteBatch;

use super::{dup_entry, entries_for_row, IndexOps, IndexRef, RowSide};
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::schema::{TableSchema, Value};
use crate::store::Store;

/// One row transition of a write batch (old absent = insert, new absent
/// = delete; a pk-moving UPDATE carries both pk identities).
#[derive(Debug, Clone, Copy)]
pub struct Transition<'a> {
    pub old: Option<RowSide<'a>>,
    pub new: Option<RowSide<'a>>,
}

impl<'a> Transition<'a> {
    pub fn insert(new: RowSide<'a>) -> Transition<'a> {
        Transition {
            old: None,
            new: Some(new),
        }
    }

    pub fn delete(old: RowSide<'a>) -> Transition<'a> {
        Transition {
            old: Some(old),
            new: None,
        }
    }
}

/// Every index-entry op of one table's batch, with unique constraints
/// validated first (store reads happen only for unique puts).
pub fn batch_ops(
    store: &Store,
    schema: &TableSchema,
    transitions: &[Transition<'_>],
) -> SqlResult<IndexOps> {
    let mut ops: IndexOps = Vec::new();
    for def in &schema.indexes {
        let index = IndexRef::of(def);
        let mut per_row: Vec<IndexOps> = Vec::with_capacity(transitions.len());
        for t in transitions {
            per_row.push(entries_for_row(schema, &index, t.old, t.new).map_err(SqlError::from)?);
        }
        if index.unique {
            check_unique(store, schema, &index, &per_row)?;
        }
        ops.extend(per_row.into_iter().flatten());
    }
    Ok(ops)
}

/// Unique validation of one index over the batch (see module doc).
/// `per_row` holds this index's ops per transition, in batch order.
fn check_unique(
    store: &Store,
    schema: &TableSchema,
    index: &IndexRef,
    per_row: &[IndexOps],
) -> SqlResult<()> {
    // key -> the pk claiming it after the batch (delete = unclaimed).
    let mut claimed: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
    let mut vacated: BTreeSet<Vec<u8>> = BTreeSet::new();
    for ops in per_row {
        for (key, val) in ops {
            match val {
                None => {
                    vacated.insert(key.clone());
                    claimed.insert(key.clone(), None);
                }
                Some(pk) => {
                    if let Some(Some(other)) = claimed.get(key) {
                        if other != pk {
                            return Err(dup_entry(&value_of_key(schema, index, key), &index.name));
                        }
                    }
                    claimed.insert(key.clone(), Some(pk.clone()));
                }
            }
        }
    }
    // Store check for the surviving claims, skipping keys this batch
    // vacates (their on-disk owner is leaving in the same batch).
    for (key, owner) in &claimed {
        let Some(pk) = owner else { continue };
        if vacated.contains(key) {
            continue;
        }
        if let Some(disk) = unique_owner_by_key(store, key)? {
            if &disk != pk {
                return Err(dup_entry(&value_of_key(schema, index, key), &index.name));
            }
        }
    }
    Ok(())
}

/// Decode the indexed value back out of an entry key (error-message
/// rendering only; NULL when undecodable).
fn value_of_key(schema: &TableSchema, index: &IndexRef, key: &[u8]) -> Value {
    let ty = schema
        .columns
        .get(super::column_pos(schema, index).unwrap_or(usize::MAX))
        .map(|c| c.sql_type);
    keys_parse(key, ty).unwrap_or(Value::Null)
}

fn keys_parse(key: &[u8], ty: Option<crate::sql::storage::schema::SqlType>) -> Option<Value> {
    let (_, _, _, tail) = super::keys::parse_index_key(key)?;
    super::keys::value_of_tail(&tail, ty?)
}

/// Point get of a unique entry's owner by raw key (no value re-encode).
fn unique_owner_by_key(store: &Store, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
    Ok(crate::store::ops::get_physical(store, key)?.filter(|v| !v.is_empty()))
}

/// Append index ops to a row batch: puts carry their value (pk for
/// unique, empty for secondary), deletes remove the entry key.
pub fn apply_ops(batch: &mut WriteBatch, ops: IndexOps) {
    for (key, val) in ops {
        match val {
            Some(v) => {
                batch.put(key, v);
            }
            None => {
                batch.delete(key);
            }
        }
    }
}

/// Duplicate detection for a CREATE UNIQUE INDEX backfill scan: fails
/// when two live rows share a non-null value of the indexed column.
pub fn assert_no_duplicates(
    schema: &TableSchema,
    index: &IndexRef,
    rows: &[Vec<Value>],
) -> SqlResult<()> {
    let pos = super::column_pos(schema, index).map_err(SqlError::from)?;
    let mut owners: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for row in rows {
        let v = &row[pos];
        if matches!(v, Value::Null) {
            continue; // NULLs never conflict
        }
        let ck = super::keys::col_key_of(v).map_err(SqlError::from)?;
        let pk =
            crate::sql::storage::row::pk_encode(&row[schema.pk_index()]).map_err(SqlError::from)?;
        if let Some(other) = owners.get(&ck) {
            if other != &pk {
                return Err(dup_entry(v, &index.name));
            }
        }
        owners.insert(ck, pk);
    }
    Ok(())
}
