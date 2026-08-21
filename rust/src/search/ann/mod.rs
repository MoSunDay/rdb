//! SPANN-style disk ANN for FT VECTOR fields: centroids (plus SQ8
//! calibration) live in ONE memory-sized record per field, quantized
//! posting entries live on disk per partition, and a KNN query probes
//! the `nprobe` nearest partitions, ranks candidates by SQ8 distance
//! and RERANKS the finalists with the exact vectors from the doc
//! records (exact top-k semantics restored for the finalists).
//!
//! Partition membership is maintained incrementally by FT.ADD/FT.DEL
//! (`partition`); FT.BUILD retrains centroids + calibration over the
//! whole corpus (docs added later clamp into the existing
//! calibration). One vector field per index (v1). Centroid `members`
//! counts are build-time snapshots (informational; incremental adds do
//! not maintain them).

mod kmeans;
mod partition;

pub use kmeans::{nearest, train};
pub use partition::{assign, docid_of_key, partition_add, partition_remove};

use rocksdb::WriteBatch;

use crate::store::{ops, Store};

use super::bm25::TopK;
use super::index_codec::{
    ann_posting_key, ann_posting_range, centroid_key, decode_doc, doc_key, doc_range,
    encode_centroids, CentroidTable, IndexMeta,
};
use super::quant;
use super::vecmath;

use partition::{calibration_of, encode_partition, read_partition, read_table};

/// One quantized partition entry (SQ8 bytes under the field-global
/// calibration that rides in the centroid record).
#[derive(Debug, Clone, PartialEq)]
pub struct SqEntry {
    pub docid: Vec<u8>,
    pub q: Vec<u8>,
}

/// One indexed vector doc collected by FT.BUILD / brute-force KNN.
struct VecDoc {
    docid: Vec<u8>,
    vector: Vec<f64>,
}

/// Scan the index's doc records for vector docs (physical key order).
fn scan_vec_docs(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    dim: u64,
) -> Result<Vec<VecDoc>, String> {
    let (lower, upper) = doc_range(prefix, index);
    let prefix_len = prefix.len();
    let mut out = Vec::new();
    ops::for_each_from(store, &lower, false, &mut |k, v| {
        if !upper.is_empty() && k >= upper.as_slice() {
            return false;
        }
        if let (Some(docid), Some(rec)) = (docid_of_key(k, prefix_len), decode_doc(v)) {
            if rec.vector.len() == dim as usize {
                out.push(VecDoc {
                    docid,
                    vector: rec.vector,
                });
            }
        }
        true
    })?;
    Ok(out)
}

/// FT.BUILD: retrain centroids + calibration over every vector doc,
/// re-assign partitions and stamp each doc record's centroid id. Text
/// postings are untouched; previous partitions are range-deleted.
pub fn build_batch(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    k: usize,
    iters: usize,
    seed: u64,
) -> Result<WriteBatch, String> {
    let Some((field, dim)) = super::ft_index::vector_field(meta) else {
        return Err("ERR index has no VECTOR field".to_string());
    };
    let docs = scan_vec_docs(store, prefix, index, dim)?;
    if docs.is_empty() {
        return Err("ERR no vector documents".to_string());
    }
    let vectors: Vec<Vec<f64>> = docs.iter().map(|d| d.vector.clone()).collect();
    let centroids_f32 = train(&vectors, dim as usize, k, iters, seed);
    let cal = quant::fit(&vectors, dim as usize);
    let centroids_f64: Vec<Vec<f64>> = centroids_f32
        .iter()
        .map(|c| c.iter().map(|&x| f64::from(x)).collect())
        .collect();
    let mut partitions: Vec<Vec<SqEntry>> = vec![Vec::new(); centroids_f64.len()];
    let mut members = vec![0u64; centroids_f64.len()];
    let mut batch = WriteBatch::default();
    for d in &docs {
        let (cid, _) = nearest(&centroids_f64, &d.vector);
        partitions[cid].push(SqEntry {
            docid: d.docid.clone(),
            q: quant::encode(&cal, &d.vector),
        });
        members[cid] += 1;
        let key = doc_key(prefix, index, &d.docid);
        if let Ok(Some(raw)) = ops::get_physical(store, &key) {
            if let Some(mut rec) = decode_doc(&raw) {
                rec.centroid = cid as u64;
                batch.put(key, super::index_codec::encode_doc(&rec));
            }
        }
    }
    let (lo, hi) = ann_posting_range(prefix, index, &field);
    batch.delete_range(lo, hi);
    for (cid, entries) in partitions.iter().enumerate() {
        if entries.is_empty() {
            continue;
        }
        batch.put(
            ann_posting_key(prefix, index, &field, cid as u64),
            encode_partition(dim, entries),
        );
    }
    batch.put(
        centroid_key(prefix, index, &field),
        encode_centroids(&CentroidTable {
            dim,
            centroids: centroids_f32,
            min: cal.min,
            scale: cal.scale,
            members,
        }),
    );
    Ok(batch)
}

#[allow(clippy::too_many_arguments)] // explicit slices beat a param struct here
/// KNN over the vector field: SPANN probe + exact rerank when the
/// centroid table exists, exact brute-force scan otherwise (small
/// corpora, or before the first FT.BUILD). Returns `(docid, l2)` pairs
/// best-first.
pub fn knn(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    dim: u64,
    query: &[f64],
    k: usize,
    nprobe: usize,
) -> Result<Vec<(Vec<u8>, f64)>, String> {
    let Some(table) = read_table(store, prefix, index, field)? else {
        return brute_force(store, prefix, index, dim, query, k);
    };
    let cal = calibration_of(&table);
    let centroids_f64: Vec<Vec<f64>> = table
        .centroids
        .iter()
        .map(|c| c.iter().map(|&x| f64::from(x)).collect())
        .collect();
    let nprobe = nprobe.clamp(1, centroids_f64.len());
    let mut order: Vec<(f64, usize)> = centroids_f64
        .iter()
        .enumerate()
        .map(|(i, c)| (vecmath::l2(c, query), i))
        .collect();
    order.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    // SQ8 shortlist from the probed partitions, then exact rerank of
    // the finalists from the doc records' raw vectors.
    let shortlist = k.saturating_mul(4).max(k + 16);
    let mut stage1 = TopK::new(shortlist);
    for (_, cid) in order.iter().take(nprobe) {
        for e in read_partition(store, prefix, index, field, *cid as u64)? {
            let d = quant::l2_dequant(&cal, &e.q, query);
            stage1.push(&e.docid, -d); // TopK ranks score-desc: negate distance
        }
    }
    let mut ranked = TopK::new(k);
    for hit in stage1.finish() {
        if let Ok(Some(raw)) = ops::get_physical(store, &doc_key(prefix, index, &hit.docid)) {
            if let Some(rec) = decode_doc(&raw) {
                if rec.vector.len() == dim as usize {
                    ranked.push(&hit.docid, -vecmath::l2(&rec.vector, query));
                }
            }
        }
    }
    Ok(ranked
        .finish()
        .into_iter()
        .map(|h| (h.docid, -h.score))
        .collect())
}

/// Exact scan over every vector doc (no ANN index yet / tiny corpus).
fn brute_force(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    dim: u64,
    query: &[f64],
    k: usize,
) -> Result<Vec<(Vec<u8>, f64)>, String> {
    let docs = scan_vec_docs(store, prefix, index, dim)?;
    let mut top = TopK::new(k);
    for d in &docs {
        top.push(&d.docid, -vecmath::l2(&d.vector, query));
    }
    Ok(top
        .finish()
        .into_iter()
        .map(|h| (h.docid, -h.score))
        .collect())
}
