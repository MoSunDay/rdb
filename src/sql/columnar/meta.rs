//! Segment meta records: the small JSON blobs RocksDB keeps for every
//! immutable columnar segment (kind 0x23). The segment DATA never
//! enters RocksDB -- only these metas, which name the file, carry the
//! commit ts (visibility) and the per-column zonemaps.
//!
//! Meta keys deliberately have NO slot prefix (`0x23 | table_id BE |
//! segment_id BE`): metas are never slot-routed, and a rebuild after a
//! crash is one contiguous prefix scan over the 0x23 region.

use serde::{Deserialize, Serialize};

use crate::sql::storage::codec::KIND_SQL_SEGMENT;
use crate::sql::storage::schema::Value;

/// Sanity cap for one encoded meta (the zonemaps keep them tiny).
pub const MAX_META_BYTES: usize = 1024 * 1024;

/// Lifecycle of a segment meta. `Prepared` = staged by a 2PC Prepare
/// batch (invisible until the commit flip); `Live` = committed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SegmentState {
    Prepared,
    Live,
}

/// Per-column zonemap snapshot recorded in the meta (schema order).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SegmentColumnZone {
    pub name: String,
    pub null_count: u64,
    pub min: Value,
    pub max: Value,
}

/// One immutable segment's meta record (the RocksDB 0x23 value).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SegmentMeta {
    pub table_id: u32,
    pub table_name: String,
    pub segment_id: u64,
    pub commit_ts: u64,
    pub state: SegmentState,
    pub num_rows: u64,
    /// File name under `columnar_dir` (never an absolute path).
    pub file: String,
    /// One entry per table column, schema order.
    pub columns: Vec<SegmentColumnZone>,
}

/// Placement slot of one segment: the CRC slot of the logical
/// `(table_id, segment_id)` identity. Used ONLY for data-placement
/// decisions (which node owns the flush), never to build keys.
pub fn segment_slot(table_id: u32, segment_id: u64) -> u16 {
    let mut buf = Vec::with_capacity(12);
    buf.extend_from_slice(&table_id.to_be_bytes());
    buf.extend_from_slice(&segment_id.to_be_bytes());
    crate::hash::slot_number(&buf)
}

/// Physical RocksDB key of one meta: `[0x23] | table_id BE | segment_id
/// BE` (13 bytes, deliberately slot-less -- see module doc).
pub fn meta_key(table_id: u32, segment_id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(13);
    key.push(KIND_SQL_SEGMENT);
    key.extend_from_slice(&table_id.to_be_bytes());
    key.extend_from_slice(&segment_id.to_be_bytes());
    key
}

/// Inverse of [`meta_key`]: exact 13-byte shape with the 0x23 kind.
pub fn parse_meta_key(key: &[u8]) -> Option<(u32, u64)> {
    if key.len() != 13 || key[0] != KIND_SQL_SEGMENT {
        return None;
    }
    let table_id = u32::from_be_bytes(key[1..5].try_into().unwrap());
    let segment_id = u64::from_be_bytes(key[5..13].try_into().unwrap());
    Some((table_id, segment_id))
}

/// JSON encoding of one meta (with the size sanity cap).
pub fn encode_meta(meta: &SegmentMeta) -> Result<Vec<u8>, String> {
    let out = serde_json::to_vec(meta).map_err(|e| format!("segment meta serialize: {e}"))?;
    if out.len() > MAX_META_BYTES {
        return Err(format!("segment meta too large ({} bytes)", out.len()));
    }
    Ok(out)
}

/// JSON decoding of one meta (rejects oversized input up front).
pub fn decode_meta(bytes: &[u8]) -> Result<SegmentMeta, String> {
    if bytes.len() > MAX_META_BYTES {
        return Err(format!("segment meta too large ({} bytes)", bytes.len()));
    }
    serde_json::from_slice(bytes).map_err(|e| format!("segment meta json: {e}"))
}
