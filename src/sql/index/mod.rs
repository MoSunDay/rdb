//! Secondary/unique index maintenance layer (M2): pure key-op derivation
//! plus store lookups. Physical layouts live in `keys.rs`; the
//! batch-level unique-constraint checking shared by the autocommit and
//! transactional write paths lives in `maintain.rs`.
//!
//! Index entries are derived, never staged: every write path (autocommit
//! batch, txn COMMIT, CREATE INDEX backfill) computes the SAME pure
//! function `entries_for_row(old, new)` over old-vs-new row values and
//! commits the ops in the same RocksDB batch as the row versions.

pub mod keys;
pub mod maintain;

use crate::sql::parse::error::SqlError;
use crate::sql::storage::codec::KIND_SQL_UNIQUE_INDEX;
use crate::sql::storage::row;
use crate::sql::storage::schema::{SqlType, TableSchema, Value};
use crate::store::ops;
use crate::store::rocksdb::key_upper_bound;
use crate::store::Store;

/// One index-entry write: `Some(value)` = put, `None` = delete.
pub type IndexOp = (Vec<u8>, Option<Vec<u8>>);
pub type IndexOps = Vec<IndexOp>;

/// One side of a row transition: the row's pk identity + full values.
#[derive(Debug, Clone, Copy)]
pub struct RowSide<'a> {
    pub pk_key: &'a [u8],
    pub values: &'a [Value],
}

/// Column position (the stable column id: positions never move, no
/// ALTER exists) of one index's column.
pub fn column_pos(schema: &TableSchema, index: &IndexRef) -> Result<usize, String> {
    schema
        .column_index(&index.column)
        .ok_or_else(|| format!("index column '{}' missing from table", index.column))
}

/// Borrowed index metadata the maintenance layer needs (from `IndexDef`).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexRef {
    pub name: String,
    pub column: String,
    pub unique: bool,
}

impl IndexRef {
    pub fn of(index: &crate::sql::storage::schema::IndexDef) -> IndexRef {
        IndexRef {
            name: index.name.clone(),
            column: index.column.clone(),
            unique: index.unique,
        }
    }
}

/// The index-entry ops of ONE row transition:
/// `(None, Some(new))` = insert, `(Some(old), None)` = delete,
/// `(Some(old), Some(new))` = update. NULL column values are never
/// indexed. A transition that leaves both pk and indexed value
/// unchanged emits nothing. Unique entries store the owning pk as the
/// value; secondary entries append the pk to the key and store b"".
pub fn entries_for_row(
    schema: &TableSchema,
    index: &IndexRef,
    old: Option<RowSide<'_>>,
    new: Option<RowSide<'_>>,
) -> Result<IndexOps, String> {
    let col_pos = column_pos(schema, index)?;
    let ov = old.map(|s| &s.values[col_pos]);
    let nv = new.map(|s| &s.values[col_pos]);
    let non_null = non_null_of;
    if let (Some(a), Some(b)) = (non_null(ov), non_null(nv)) {
        if a == b && old.expect("a").pk_key == new.expect("b").pk_key {
            return Ok(Vec::new()); // index-invisible change
        }
    }
    let mut ops: IndexOps = Vec::with_capacity(2);
    if let (Some(side), Some(v)) = (old, non_null(ov)) {
        let ck = keys::col_key_of(v)?;
        ops.push((
            if index.unique {
                keys::unique_key(schema.id, col_pos as u32, &ck)
            } else {
                keys::secondary_key(schema.id, col_pos as u32, &ck, side.pk_key)
            },
            None,
        ));
    }
    if let (Some(side), Some(v)) = (new, non_null(nv)) {
        let ck = keys::col_key_of(v)?;
        ops.push((
            if index.unique {
                keys::unique_key(schema.id, col_pos as u32, &ck)
            } else {
                keys::secondary_key(schema.id, col_pos as u32, &ck, side.pk_key)
            },
            Some(if index.unique {
                side.pk_key.to_vec()
            } else {
                Vec::new()
            }),
        ));
    }
    Ok(ops)
}

/// NULL filter helper (NULL column values are never indexed).
fn non_null_of(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !matches!(v, Value::Null))
}

/// Entry ops for a whole live row (CREATE INDEX backfill): insert side.
pub fn entries_for_live_row(
    schema: &TableSchema,
    index: &IndexRef,
    pk_key: &[u8],
    values: &[Value],
) -> Result<IndexOps, String> {
    entries_for_row(schema, index, None, Some(RowSide { pk_key, values }))
}

/// Owning pk of one exact value under a UNIQUE index (point get).
pub fn unique_owner(
    store: &Store,
    schema: &TableSchema,
    index: &IndexRef,
    value: &Value,
) -> Result<Option<Vec<u8>>, String> {
    if matches!(value, Value::Null) || !index.unique {
        return Ok(None);
    }
    let col_pos = column_pos(schema, index)?;
    let key = keys::unique_key(schema.id, col_pos as u32, &keys::col_key_of(value)?);
    Ok(ops::get_physical(store, &key)?.filter(|v| !v.is_empty()))
}

/// All pks whose indexed value equals `value` (one point get for unique
/// indexes, one short prefix scan for secondary ones).
pub fn lookup_pks(
    store: &Store,
    schema: &TableSchema,
    index: &IndexRef,
    value: &Value,
) -> Result<Vec<Vec<u8>>, String> {
    if matches!(value, Value::Null) {
        return Ok(Vec::new());
    }
    let col_pos = column_pos(schema, index)?;
    let ck = keys::col_key_of(value)?;
    if index.unique {
        return Ok(unique_owner(store, schema, index, value)?
            .into_iter()
            .collect());
    }
    let prefix = keys::secondary_value_prefix(schema.id, col_pos as u32, &ck);
    let mut pks = Vec::new();
    ops::for_each_from(store, &prefix, false, &mut |key, _| {
        if !key.starts_with(&prefix) {
            return false;
        }
        // Entry keys under the prefix are exactly this (value, pks...) set.
        if let Some((_, _, _, tail)) = keys::parse_index_key(key) {
            if let Some((_, pk)) = keys::split_tail(&tail, col_ty(schema, col_pos)) {
                pks.push(pk.to_vec());
            }
        }
        true
    })?;
    Ok(pks)
}

/// All pks whose indexed value lies in `[low, high]` inclusive (key
/// order = value order inside the index's contiguous slot range, so
/// both kinds are served by one forward walk).
pub fn lookup_range(
    store: &Store,
    schema: &TableSchema,
    index: &IndexRef,
    low: &Value,
    high: &Value,
) -> Result<Vec<Vec<u8>>, String> {
    let col_pos = column_pos(schema, index)?;
    let lo = keys::col_key_of(low)?;
    let hi = keys::col_key_of(high)?;
    if lo > hi {
        return Ok(Vec::new());
    }
    let ty = col_ty(schema, col_pos);
    let kind: u8 = if index.unique {
        KIND_SQL_UNIQUE_INDEX
    } else {
        crate::sql::storage::codec::KIND_SQL_INDEX
    };
    let prefix = keys::index_prefix(schema.id, col_pos as u32, kind);
    let mut from = prefix.clone();
    from.extend_from_slice(&lo);
    let mut pks = Vec::new();
    ops::for_each_from(store, &from, false, &mut |key, val| {
        if !key.starts_with(&prefix) {
            return false; // walked off the index keyspace
        }
        let Some((k, _, _, tail)) = keys::parse_index_key(key) else {
            return true;
        };
        if k != kind {
            return true;
        }
        match keys::split_tail(&tail, ty) {
            Some((ck, pk)) if ck <= hi.as_slice() => {
                if !index.unique {
                    pks.push(pk.to_vec());
                } else if !val.is_empty() {
                    pks.push(val.to_vec());
                }
                true
            }
            // past high: entries are value-ordered, nothing more matches
            Some(_) => false,
            None => true,
        }
    })?;
    Ok(pks)
}

/// Storage type of column `pos`.
fn col_ty(schema: &TableSchema, pos: usize) -> SqlType {
    schema.columns[pos].sql_type
}

/// The live row of one pk visible at `read_ts` (a seek at the version
/// prefix + `visible_value` semantics), or None when the pk has no
/// visible live version. Shared by IndexLookup row fetches and by
/// COMMIT-time old-row recovery for index derivation.
pub fn visible_row_at_pk(
    store: &Store,
    schema: &TableSchema,
    pk_key: &[u8],
    read_ts: u64,
) -> Result<Option<Vec<Value>>, String> {
    let prefix = row::version_prefix(schema.id, pk_key);
    let mut visible: Option<Vec<u8>> = None;
    ops::for_each_from(store, &prefix, false, &mut |key, val| {
        if !key.starts_with(&prefix) {
            return false;
        }
        if let Some((_, _, _, ts)) = row::parse_version_key(key) {
            if ts <= read_ts && !row::is_prepared(val) {
                visible = Some(val.to_vec());
                return false; // newest visible version: stop
            }
        }
        true
    })?;
    match visible {
        None => Ok(None),
        Some(raw) => {
            let (header, values) = row::decode_version(schema, &raw)?;
            Ok((header == row::HEADER_LIVE).then_some(values))
        }
    }
}

/// Delete cap per page of [`drop_entries`] (bounded, resumable work).
pub const DROP_PAGE: usize = 10_000;

/// One bounded page of the DROP INDEX entry sweep: deletes index keys of
/// `table_id` / `col_pos` (either kind) starting at `cursor`, returning
/// (deleted, next-cursor). [`keys::sweep_start`] is the first cursor;
/// deleted == 0 means the walk is finished. The sweep covers exactly
/// the index's own slot (the parse filter keeps foreign keys of the
/// same slot safe).
pub fn drop_entries_page(
    store: &Store,
    table_id: u32,
    col_pos: u32,
    cursor: &[u8],
) -> Result<(usize, Vec<u8>), String> {
    let end = key_upper_bound(&keys::sweep_start(table_id, col_pos)).unwrap_or_default();
    let bounded = !end.is_empty();
    let mut dead: Vec<Vec<u8>> = Vec::new();
    let mut next = cursor.to_vec();
    ops::for_each_from(store, cursor, false, &mut |key, _| {
        next = key.to_vec();
        if bounded && key >= end.as_slice() {
            return false;
        }
        if let Some((_, t, c, _)) = keys::parse_index_key(key) {
            if t == table_id && c == col_pos && dead.len() < DROP_PAGE {
                dead.push(key.to_vec());
            }
        }
        dead.len() < DROP_PAGE
    })?;
    if dead.is_empty() {
        return Ok((0, Vec::new()));
    }
    let n = dead.len();
    let mut batch = rocksdb::WriteBatch::default();
    for k in &dead {
        batch.delete(k);
    }
    ops::batch_write(store, batch)?;
    Ok((n, next))
}

/// Full DROP INDEX entry sweep: every 0x21/0x22 key of the table/column,
/// deleted page by page (each page one synced batch).
pub async fn drop_entries(
    store: std::sync::Arc<Store>,
    table_id: u32,
    col_pos: u32,
) -> Result<usize, String> {
    let mut cursor = keys::sweep_start(table_id, col_pos);
    let mut total = 0usize;
    loop {
        let (deleted, next) = {
            let from = cursor.clone();
            let store = std::sync::Arc::clone(&store);
            tokio::task::spawn_blocking(move || drop_entries_page(&store, table_id, col_pos, &from))
                .await
                .map_err(|e| e.to_string())??
        };
        tokio::task::yield_now().await;
        if deleted == 0 {
            return Ok(total);
        }
        total += deleted;
        cursor = next;
    }
}

/// Duplicate-entry error in MySQL shape:
/// "Duplicate entry 'v' for key 'idx'".
pub fn dup_entry(value: &Value, index: &str) -> SqlError {
    SqlError::new(
        crate::sql::parse::error::ErrorCode::DupEntry,
        format!(
            "Duplicate entry {} for key '{}'",
            keys::value_display(value),
            index
        ),
    )
}

#[cfg(test)]
mod tests;
