//! COMMIT-time flush of columnar append buffers.
//!
//! Explicit-txn commits stage appends into the [`crate::sql::tx::Txn`]
//! buffer at INSERT and flush each table's rows here: one immutable
//! segment file + one `Live` meta, whose RocksDB put joins the txn's
//! row writes in the SAME atomic commit batch (the caller owns the
//! batch; this module never writes RocksDB itself).

use crate::sql::columnar::meta::{SegmentMeta, SegmentState};
use crate::sql::columnar::writer;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{TableSchema, Value};
use crate::state::Shared;

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
