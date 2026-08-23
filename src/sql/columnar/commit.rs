//! COMMIT-time flush of columnar append buffers.
//!
//! Explicit-txn commits stage appends into the [`crate::sql::tx::Txn`]
//! buffer at INSERT and flush each table's rows here: one immutable
//! segment file + one `Live` meta, whose RocksDB put joins the txn's
//! row writes in the SAME atomic commit batch (the caller owns the
//! batch; this module never writes RocksDB itself).

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::columnar::meta::{self, SegmentMeta, SegmentState};
use crate::sql::columnar::registry_of;
use crate::sql::columnar::writer;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::codec::KIND_SQL_SEGMENT;
use crate::sql::storage::schema::{TableSchema, Value};
use crate::state::Shared;
use crate::store::ops;

/// One table's appended rows, planned into a single segment. Lives only
/// on the coordinator/participant that owns the flush -- never serialized.
#[derive(Clone, Debug)]
pub struct PendingSegment {
    pub schema: TableSchema,
    pub segment_id: u64,
    pub commit_ts: u64,
    pub rows: Vec<Vec<Value>>,
}

/// Local-commit flush used by `tx::session` when no 2PC is needed.
///
/// Writes the segment file and returns the `Live` meta; does NOT write
/// RocksDB or the registry (the caller batches the meta put atomically
/// with the txn's row writes and registers after the batch lands).
pub fn flush_appends(
    shared: &Shared,
    table_name: &str,
    rows: &[Vec<Value>],
    commit_ts: u64,
) -> SqlResult<SegmentMeta> {
    let schema = catalog::lookup(shared, table_name)
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?
        .ok_or_else(|| {
            SqlError::new(
                ErrorCode::NoSuchTable,
                format!("table '{table_name}' dropped mid-txn"),
            )
        })?;
    if !schema.engine.is_columnar() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("table '{table_name}' is not columnar"),
        ));
    }
    let segment_id = writer::local_segment_id(shared, schema.id, commit_ts)
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
    let dir = writer::columnar_dir(&shared.conf);
    let (file, columns, num_rows) = writer::write_segment_file(&dir, &schema, segment_id, rows)
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
    Ok(writer::build_meta(
        &schema,
        segment_id,
        commit_ts,
        SegmentState::Live,
        file,
        columns,
        num_rows,
    ))
}

/// Physical cleanup of a dropped columnar table: delete every 0x23
/// meta key of `table_id` in ONE batch, drop the registry entries,
/// then remove the segment files (best effort). Called AFTER the
/// catalog drop succeeds; whatever a crash leaves behind, the M5
/// sweep collects. A Prepared segment decided-commit after the drop
/// can re-insert a meta for a table that no longer exists -- the
/// sweep treats metas of unknown tables as garbage.
pub async fn drop_table_segments(shared: &Shared, table_id: u32) -> SqlResult<()> {
    // One table's metas are contiguous: `0x23 | table_id BE | segment
    // BE`, so one forward walk from the 5-byte prefix covers exactly
    // this table (the next table's first meta ends the scan).
    let mut prefix = Vec::with_capacity(5);
    prefix.push(KIND_SQL_SEGMENT);
    prefix.extend_from_slice(&table_id.to_be_bytes());
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    ops::for_each_from(&shared.store, &prefix, false, &mut |key, value| {
        if !key.starts_with(&prefix) {
            return false; // past this table's meta region
        }
        keys.push(key.to_vec());
        match meta::decode_meta(value) {
            Ok(m) => files.push(m.file),
            Err(e) => eprintln!("columnar: drop: undecodable segment meta: {e}"),
        }
        true
    })
    .map_err(SqlError::from)?;
    if !keys.is_empty() {
        let mut batch = WriteBatch::default();
        for key in &keys {
            batch.delete(key);
        }
        ops::batch_write_async(Arc::clone(&shared.store), batch)
            .await
            .map_err(SqlError::from)?;
    }
    registry_of(shared).drop_table(table_id);
    for file in &files {
        writer::delete_file(&shared.conf, file);
    }
    Ok(())
}
