//! M3 read path over columnar segments: decode the Live segments the
//! registry tracks for a table and concatenate them into plain rows.
//!
//! Visibility is segment-level: a scan returns every `Live` segment
//! with `commit_ts <= read_ts` plus, for an open txn, its own staged
//! appends. Prepared 2PC segments never reach the registry, hence
//! never appear here. Columnar tables have no pk ordering: rows come
//! back in segment order, insertion order within a segment.

use crate::sql::columnar::decode;
use crate::sql::columnar::meta::SegmentState;
use crate::sql::columnar::registry_of;
use crate::sql::columnar::writer;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{TableSchema, Value};
use crate::state::Shared;

/// Decode one segment file's bytes into full-width rows (schema order).
pub fn decode_segment(bytes: &[u8], schema: &TableSchema) -> SqlResult<Vec<Vec<Value>>> {
    let (footer, _region) =
        decode::open(bytes).map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
    if footer.columns.len() != schema.columns.len() {
        return Err(SqlError::new(
            ErrorCode::Unknown,
            format!(
                "columnar segment width mismatch: file has {} columns, table '{}' has {}",
                footer.columns.len(),
                schema.name,
                schema.columns.len()
            ),
        ));
    }
    let num_rows = usize::try_from(footer.num_rows)
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))?;
    let mut columns = Vec::with_capacity(footer.columns.len());
    for (col, def) in footer.columns.iter().zip(&schema.columns) {
        // Columns decode by POSITION with the schema's type for that
        // ordinal; a ragged column is a corrupt segment.
        let values = decode::decode_column(bytes, col, def.sql_type)
            .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
        if values.len() != num_rows {
            return Err(SqlError::new(
                ErrorCode::Unknown,
                format!(
                    "columnar segment column '{}' is ragged: {} values for {} rows",
                    col.name,
                    values.len(),
                    num_rows
                ),
            ));
        }
        columns.push(values);
    }
    let mut rows = Vec::with_capacity(num_rows);
    for r in 0..num_rows {
        rows.push(columns.iter().map(|c| c[r].clone()).collect());
    }
    Ok(rows)
}

/// Locally visible rows of one columnar table at `read_ts`; `overlay`
/// (the open txn's staged appends for this table, if any) is appended
/// last.
///
/// Every `Live` segment with `commit_ts <= read_ts` contributes its
/// rows, segments ordered by `(commit_ts, segment_id)`, rows within a
/// segment in insertion order. Note the registry insert happens just
/// after the commit batch lands, so a same-node reader can miss a
/// just-committed segment for a few µs (benign: a brief miss of
/// committed data, never uncommitted data).
pub fn scan_local(
    shared: &Shared,
    schema: &TableSchema,
    read_ts: u64,
    overlay: Option<&[Vec<Value>]>,
) -> SqlResult<Vec<Vec<Value>>> {
    let mut metas: Vec<_> = registry_of(shared)
        .segments(schema.id)
        .into_iter()
        .filter(|m| m.state == SegmentState::Live && m.commit_ts <= read_ts)
        .collect();
    metas.sort_by_key(|m| (m.commit_ts, m.segment_id));
    let dir = writer::columnar_dir(&shared.conf);
    let mut rows = Vec::new();
    for m in &metas {
        let bytes = std::fs::read(dir.join(&m.file)).map_err(|e| {
            SqlError::new(
                ErrorCode::Unknown,
                format!("columnar segment {}: {e}", m.file),
            )
        })?;
        rows.extend(decode_segment(&bytes, schema)?);
    }
    if let Some(ov) = overlay {
        rows.extend(ov.iter().cloned());
    }
    Ok(rows)
}

/// Participant-side answer to `ScanColumnar`: schema by id (like
/// `scan::band_rows`), must be columnar, local scan without overlay.
pub fn table_rows(shared: &Shared, table_id: u32, read_ts: u64) -> SqlResult<Vec<Vec<Value>>> {
    let schema = catalog::list_tables(shared)
        .into_iter()
        .find(|s| s.id == table_id)
        .ok_or_else(|| SqlError::new(ErrorCode::Unknown, format!("table id {table_id} unknown")))?;
    if !schema.engine.is_columnar() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("table '{}' is not columnar", schema.name),
        ));
    }
    scan_local(shared, &schema, read_ts, None)
}
