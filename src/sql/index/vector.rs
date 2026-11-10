//! Physical layout of SQL vector (ANN) index entries (P4, 02 doc §4.1).
//!
//! A vector index shares the secondary-index slot rule
//! (`keys::index_slot(table_id, col_pos)`), so its whole entry family is
//! one contiguous keyspace, but under DEDICATED kinds:
//!
//! ```text
//! meta:     "<slot/>" 0x24 table_id col_pos                     -> VecIndexMeta
//! centroid: "<slot/>" 0x25 table_id col_pos                     -> k*dim f32LE
//! posting:  "<slot/>" 0x26 table_id col_pos centroid_id(u32 BE) -> postings blob
//! ```
//!
//! Like 0x21/0x22, entries carry NO timestamps: they are derived
//! latest-state pointers rewritten in the same RocksDB batch as the
//! rows they mirror. The posting blob is `count(u32) ++ per entry
//! [pk_len(u16) pk_key sq8(dim bytes)]` with pk keys bytewise-sorted;
//! SQ8 codes use the search kernel's per-dim `Calibration` stored in
//! the meta record.

use crate::search::quant::Calibration;
use crate::sql::index::keys::{head, index_slot};
use crate::sql::storage::codec::{KIND_SQL_VEC_CENTROID, KIND_SQL_VEC_META, KIND_SQL_VEC_POSTING};
use crate::store::rocksdb::slot_prefix;

/// Distance metric a vector index is trained and probed with. The wire
/// spelling matches the 3-arg DISTANCE face ('dot' is negated inner
/// product there; the index stores it as plain dot and negates at the
/// probe, keeping coarse ranking monotone in both).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Cosine,
    Euclidean,
    Dot,
}

impl Metric {
    pub fn parse(s: &str) -> Option<Metric> {
        match s.to_ascii_lowercase().as_str() {
            "cosine" => Some(Metric::Cosine),
            "euclidean" => Some(Metric::Euclidean),
            "dot" => Some(Metric::Dot),
            _ => None,
        }
    }

    pub fn tag(self) -> u8 {
        match self {
            Metric::Cosine => 0,
            Metric::Euclidean => 1,
            Metric::Dot => 2,
        }
    }

    fn from_tag(tag: u8) -> Option<Metric> {
        match tag {
            0 => Some(Metric::Cosine),
            1 => Some(Metric::Euclidean),
            2 => Some(Metric::Dot),
            _ => None,
        }
    }
}

/// Catalog-carried parameters of one vector index. `dim` is copied from
/// the column type at DDL time so the physical records stay
/// self-describing even if the column is later dropped with the table.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VectorIndexParams {
    pub metric: Metric,
    pub dim: u32,
    pub nprobe: u16,
}

/// Build-time constants: centroid count is clamped around sqrt(n)
/// (SPANN rule of thumb); kmeans iterations are low because the exact
/// rerank absorbs coarse error.
pub fn centroid_count(rows: usize) -> usize {
    rows.isqrt().clamp(1, 64)
}

pub const KMEANS_ITERS: usize = 8;
pub const DEFAULT_NPROBE: u16 = 4;

pub fn meta_key(table_id: u32, col_pos: u32) -> Vec<u8> {
    head(
        index_slot(table_id, col_pos),
        KIND_SQL_VEC_META,
        table_id,
        col_pos,
    )
}

pub fn centroid_key(table_id: u32, col_pos: u32) -> Vec<u8> {
    head(
        index_slot(table_id, col_pos),
        KIND_SQL_VEC_CENTROID,
        table_id,
        col_pos,
    )
}

pub fn posting_key(table_id: u32, col_pos: u32, centroid: u32) -> Vec<u8> {
    let mut k = head(
        index_slot(table_id, col_pos),
        KIND_SQL_VEC_POSTING,
        table_id,
        col_pos,
    );
    k.extend_from_slice(&centroid.to_be_bytes());
    k
}

/// Prefix of every posting of one index (walks probe partitions in
/// centroid-id order).
pub fn posting_prefix(table_id: u32, col_pos: u32) -> Vec<u8> {
    head(
        index_slot(table_id, col_pos),
        KIND_SQL_VEC_POSTING,
        table_id,
        col_pos,
    )
}

/// Inclusive start of the index's whole slot: the DROP sweep cursor.
pub fn sweep_start(table_id: u32, col_pos: u32) -> Vec<u8> {
    slot_prefix(index_slot(table_id, col_pos))
}

pub fn is_vector_kind(kind: u8) -> bool {
    kind == KIND_SQL_VEC_META || kind == KIND_SQL_VEC_CENTROID || kind == KIND_SQL_VEC_POSTING
}

/// One posting entry: owning row pk plus its SQ8 code.
#[derive(Debug, Clone, PartialEq)]
pub struct Posting {
    pub pk: Vec<u8>,
    pub sq8: Vec<u8>,
}

/// meta value: `metric u8 ++ dim u32 ++ nprobe u16 ++ k u16 ++
/// calibration (dim* f32LE min ++ dim* f32LE scale)`.
pub fn encode_meta(p: &VectorIndexParams, k: usize, cal: &Calibration) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + cal.min.len() * 8);
    v.push(p.metric.tag());
    v.extend_from_slice(&p.dim.to_be_bytes());
    v.extend_from_slice(&p.nprobe.to_be_bytes());
    v.extend_from_slice(&(k as u16).to_be_bytes());
    for f in cal.min.iter().chain(cal.scale.iter()) {
        v.extend_from_slice(&f.to_le_bytes());
    }
    v
}

pub fn decode_meta(v: &[u8]) -> Option<(VectorIndexParams, usize, Calibration)> {
    if v.len() < 12 {
        return None;
    }
    let metric = Metric::from_tag(v[0])?;
    let dim = u32::from_be_bytes(v[1..5].try_into().ok()?);
    let nprobe = u16::from_be_bytes(v[5..7].try_into().ok()?);
    let k = u16::from_be_bytes(v[7..9].try_into().ok()?) as usize;
    let body = &v[9..];
    if body.len() != dim as usize * 8 {
        return None;
    }
    let halves = dim as usize * 4;
    let min = body[..halves]
        .as_chunks::<4>()
        .0
        .iter()
        .map(f32_from_le)
        .collect();
    let scale = body[halves..]
        .as_chunks::<4>()
        .0
        .iter()
        .map(f32_from_le)
        .collect();
    Some((
        VectorIndexParams {
            metric,
            dim,
            nprobe,
        },
        k,
        Calibration { min, scale },
    ))
}

fn f32_from_le(c: &[u8; 4]) -> f32 {
    f32::from_le_bytes(*c)
}

/// centroids value: `k * dim` f32LE.
pub fn encode_centroids(cs: &[Vec<f32>]) -> Vec<u8> {
    let mut v = Vec::with_capacity(cs.len() * cs.first().map(|c| c.len()).unwrap_or(0) * 4);
    for c in cs {
        for f in c {
            v.extend_from_slice(&f.to_le_bytes());
        }
    }
    v
}

pub fn decode_centroids(v: &[u8], dim: usize) -> Option<Vec<Vec<f32>>> {
    if !v.len().is_multiple_of(dim * 4) {
        return None;
    }
    Some(
        v.chunks_exact(dim * 4)
            .map(|c| c.as_chunks::<4>().0.iter().map(f32_from_le).collect())
            .collect(),
    )
}

/// postings value: `count u32 ++ entries [pk_len u16 pk sq8(dim)]`,
/// entries pk-sorted (binary-search friendly for point removal).
pub fn encode_postings(entries: &[Posting]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + entries.len() * (2 + 8 + 4));
    v.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        v.extend_from_slice(&(e.pk.len() as u16).to_be_bytes());
        v.extend_from_slice(&e.pk);
        v.extend_from_slice(&e.sq8);
    }
    v
}

pub fn decode_postings(v: &[u8], dim: usize) -> Option<Vec<Posting>> {
    if v.len() < 4 {
        return None;
    }
    let count = u32::from_be_bytes(v[..4].try_into().ok()?) as usize;
    let mut out = Vec::with_capacity(count);
    let mut at = 4;
    for _ in 0..count {
        if at + 2 > v.len() {
            return None;
        }
        let plen = u16::from_be_bytes(v[at..at + 2].try_into().ok()?) as usize;
        at += 2;
        if at + plen + dim > v.len() {
            return None;
        }
        out.push(Posting {
            pk: v[at..at + plen].to_vec(),
            sq8: v[at + plen..at + plen + dim].to_vec(),
        });
        at += plen + dim;
    }
    if at != v.len() {
        return None;
    }
    Some(out)
}

/// Latch key serializing the read-modify-write window on ONE vector
/// index's physical records: DML postings transitions, the CREATE
/// backfill, and the DROP sweep all pass through it (concurrent
/// read-posting -> append -> rewrite would silently drop entries).
pub fn latch_key(table_id: u32, index_id: u32) -> Vec<u8> {
    format!("sqlvec/{table_id}/{index_id}").into_bytes()
}

/// Acquire the per-index latches for every vector index of a schema, in
/// ascending index-id order (a stable order means two statements locking
/// the same pair can never deadlock). No-op (and await-free) for tables
/// without vector indexes.
pub async fn lock_schema_indexes(
    latch: &crate::ds::latch::Latch,
    schema: &crate::sql::storage::schema::TableSchema,
) -> Vec<crate::ds::latch::KeyGuard> {
    let mut ids: Vec<u32> = schema
        .indexes
        .iter()
        .filter(|d| d.vector.is_some())
        .map(|d| d.id)
        .collect();
    ids.sort_unstable();
    let mut guards = Vec::with_capacity(ids.len());
    for id in ids {
        guards.push(crate::ds::latch::lock(latch, &latch_key(schema.id, id)).await);
    }
    guards
}
