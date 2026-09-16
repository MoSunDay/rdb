//! Segment-file writer + placement + autocommit fast path.
//!
//! One flush = one immutable file per table, written as
//! `<final>.tmp` + fsync + rename (a torn tmp never masquerades as a
//! segment), plus best-effort dir fsync. The matching RocksDB meta is
//! published by the caller's atomic WriteBatch.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::conf::Config;
use crate::sql::columnar::encode;
use crate::sql::columnar::meta::{self, SegmentColumnZone, SegmentMeta, SegmentState};
use crate::sql::columnar::registry_of;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{TableSchema, Value};
use crate::state::Shared;
use crate::store::ops;

/// Built-in default of `columnar_flush_rows` (0 in the config).
pub const DEFAULT_FLUSH_ROWS: u64 = 65_536;
/// Built-in default of `columnar_flush_bytes` (0 in the config): 64 MiB.
pub const DEFAULT_FLUSH_BYTES: u64 = 64 * 1024 * 1024;
/// Candidates tried by [`choose_segment_id`] before giving up.
const MAX_SEGMENT_ID_CANDIDATES: u64 = 1 << 20;

/// `columnar_flush_rows` accessor: 0 falls back to the built-in.
pub fn flush_rows_limit(conf: &Config) -> u64 {
    if conf.columnar_flush_rows == 0 {
        DEFAULT_FLUSH_ROWS
    } else {
        conf.columnar_flush_rows
    }
}

/// `columnar_flush_bytes` accessor: 0 falls back to the built-in.
pub fn flush_bytes_limit(conf: &Config) -> u64 {
    if conf.columnar_flush_bytes == 0 {
        DEFAULT_FLUSH_BYTES
    } else {
        conf.columnar_flush_bytes
    }
}

/// Directory holding every segment file of this instance.
pub fn columnar_dir(conf: &Config) -> std::path::PathBuf {
    crate::store::data_path(&conf.store_path, &conf.bind).join("columnar")
}

/// Deterministic file name of one segment (unique per (table, id)).
pub fn segment_file_name(table_id: u32, segment_id: u64) -> String {
    format!("t{table_id}-s{segment_id}.col")
}

/// Cheap size estimate of one buffered row for the flush-bytes limit:
/// a tag + width per scalar, plus the payload of variable-length types.
pub fn estimate_row_bytes(values: &[Value]) -> usize {
    values
        .iter()
        .map(|v| {
            1 + 8
                + match v {
                    Value::Str(s) => s.len(),
                    Value::Bytes(b) => b.len(),
                    _ => 0,
                }
        })
        .sum()
}

/// Encode `rows` into one immutable segment file and persist it
/// under `dir` (tmp + fsync + rename). Returns the file name, the
/// per-column zonemaps and the row count.
pub fn write_segment_file(
    dir: &Path,
    schema: &TableSchema,
    segment_id: u64,
    rows: &[Vec<Value>],
) -> Result<(String, Vec<SegmentColumnZone>, u64), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create columnar dir: {e}"))?;
    let (bytes, zones) = encode::build_segment(schema, rows)?;
    let name = segment_file_name(schema.id, segment_id);
    let tmp = dir.join(format!("{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create tmp segment: {e}"))?;
        f.write_all(&bytes)
            .map_err(|e| format!("write tmp segment: {e}"))?;
        f.sync_all()
            .map_err(|e| format!("fsync tmp segment: {e}"))?;
    }
    std::fs::rename(&tmp, dir.join(&name)).map_err(|e| format!("publish segment: {e}"))?;
    // Best-effort dir fsync: the rename is durable even if the dir
    // handle cannot be synced on this platform/filesystem.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    let columns = zones
        .into_iter()
        .map(|z| SegmentColumnZone {
            name: z.name,
            null_count: z.null_count,
            min: z.min,
            max: z.max,
        })
        .collect();
    Ok((name, columns, rows.len() as u64))
}

/// Assemble one [`SegmentMeta`] from its parts.
pub fn build_meta(
    schema: &TableSchema,
    segment_id: u64,
    commit_ts: u64,
    state: SegmentState,
    file: String,
    columns: Vec<SegmentColumnZone>,
    num_rows: u64,
) -> SegmentMeta {
    SegmentMeta {
        table_id: schema.id,
        table_name: schema.name.clone(),
        segment_id,
        commit_ts,
        state,
        num_rows,
        file,
        columns,
    }
}

/// First candidate `base, base+1, ...` that is `free` (no segment meta
/// exists yet) and whose placement slot satisfies `allow`. The scan
/// alone is NOT injective -- two bases can share the next eligible
/// slot band -- so `free` is what keeps segment ids unique.
pub fn choose_segment_id(
    table_id: u32,
    base: u64,
    allow: &dyn Fn(u16) -> bool,
    free: &dyn Fn(u64) -> bool,
) -> Result<u64, String> {
    for i in 0..MAX_SEGMENT_ID_CANDIDATES {
        let Some(id) = base.checked_add(i) else {
            break;
        };
        if free(id) && allow(meta::segment_slot(table_id, id)) {
            return Ok(id);
        }
    }
    Err("no eligible slot band for columnar segment placement".into())
}

/// Segment id for a local flush: in a ready cluster the segment must
/// land in a slot band this node owns; single-node accepts anything.
/// An id whose meta key already exists is never reused: the forward
/// scan from `base` is not injective, so a later flush whose base fell
/// just below an already-used id would pick that id again and its
/// meta/file/registry upsert would silently replace the old segment
/// (the store stays the source of truth, also across restarts).
pub fn local_segment_id(shared: &Shared, table_id: u32, base: u64) -> Result<u64, String> {
    // Treat store errors as taken: never overwrite on a failed read.
    let free = |id: u64| {
        matches!(
            ops::get_physical(&shared.store, &meta::meta_key(table_id, id)),
            Ok(None)
        )
    };
    let Some(r) = crate::sql::dist::routing(shared) else {
        return choose_segment_id(table_id, base, &|_| true, &free);
    };
    choose_segment_id(
        table_id,
        base,
        &|slot| crate::sql::dist::owner(&r, slot) == r.host,
        &free,
    )
}

/// Autocommit fast path: one segment + one meta key, published in ONE
/// atomic batch, then registered in the in-memory registry.
pub async fn commit_segment(
    shared: &Shared,
    schema: &TableSchema,
    segment_id: u64,
    commit_ts: u64,
    rows: &[Vec<Value>],
) -> SqlResult<SegmentMeta> {
    let dir = columnar_dir(&shared.conf);
    let (file, columns, num_rows) = write_segment_file(&dir, schema, segment_id, rows)
        .map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
    let meta = build_meta(
        schema,
        segment_id,
        commit_ts,
        SegmentState::Live,
        file,
        columns,
        num_rows,
    );
    let encoded = meta::encode_meta(&meta).map_err(|e| SqlError::new(ErrorCode::Unknown, e))?;
    let mut batch = WriteBatch::default();
    batch.put(meta::meta_key(meta.table_id, meta.segment_id), encoded);
    // Same-batch ts floor (the meta is visible at commit_ts; restart
    // clock fencing, see tx::floor).
    crate::sql::tx::floor::stamp(&mut batch, commit_ts);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)?;
    registry_of(shared).insert(&meta);
    Ok(meta)
}

/// Best-effort removal of one segment file (orphan sweep, drop table).
pub fn delete_file(conf: &Config, file_name: &str) {
    let _ = std::fs::remove_file(columnar_dir(conf).join(file_name));
}
