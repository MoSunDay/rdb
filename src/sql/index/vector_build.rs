//! Vector-index build and synchronous maintenance (02 doc §4.2).
//!
//! CREATE backfills the whole index in one derived-op list (meta +
//! centroids + postings); DML rewrites postings partition-by-partition
//! in the SAME RocksDB batch as the row versions (read-modify-write
//! over the store with a batch-local overlay, mirroring how
//! `maintain::batch_ops` derives 0x21/0x22 ops from row transitions).
//!
//! Coarse domain: k-means assignment and SQ8 codes run on the vector
//! as k-means sees it -- L2 space. For a cosine index both the indexed
//! vectors and the probe query are normalized first (on unit spheres
//! L2 order == cosine order); for dot the raw vectors are encoded and
//! the exact rerank absorbs the coarse mismatch. NULL vectors are
//! never indexed.

use std::collections::BTreeMap;

use crate::search::ann::{nearest, train};
use crate::search::quant::{self, Calibration};
use crate::sql::exec::scan;
use crate::sql::index::maintain::Transition;
use crate::sql::index::vector::{
    centroid_count, decode_centroids, decode_meta, decode_postings, encode_centroids, encode_meta,
    encode_postings, is_vector_kind, posting_key, sweep_start, Metric, Posting, VectorIndexParams,
    KMEANS_ITERS,
};
use crate::sql::index::{IndexOp, RowSide};
use crate::sql::storage::row::{self, HEADER_LIVE};
use crate::sql::storage::schema::{SqlType, TableSchema, Value};
use crate::store::ops;
use crate::store::Store;

/// Build every physical record of one vector index from the currently
/// visible rows. Deterministic for one row set (fixed seed).
pub fn build_ops(
    store: &Store,
    schema: &TableSchema,
    col_pos: usize,
    params: &VectorIndexParams,
    read_ts: u64,
) -> Result<Vec<IndexOp>, String> {
    let mut raws: Vec<(Vec<u8>, Vec<f32>)> = Vec::new();
    for (pk, version) in scan::visible_versions_between(store, schema, read_ts, 0, u16::MAX)
        .map_err(|e| e.to_string())?
    {
        let (header, values) = row::decode_version(schema, &version)?;
        if header != HEADER_LIVE {
            continue;
        }
        if let Value::Vector(xs) = &values[col_pos] {
            raws.push((pk, xs.clone()));
        }
    }
    let dim = params.dim as usize;
    let domain: Vec<Vec<f64>> = raws
        .iter()
        .map(|(_, xs)| to_domain(xs, params.metric))
        .collect();
    let k = centroid_count(domain.len());
    let (centroids, cal) = if k == 0 || domain.is_empty() {
        (
            Vec::new(),
            Calibration {
                min: vec![0.0; dim],
                scale: vec![1.0; dim],
            },
        )
    } else {
        let cs = train(&domain, dim, k, KMEANS_ITERS, 7);
        let cal = quant::fit(&domain, dim);
        (cs, cal)
    };
    let centroids_f64: Vec<Vec<f64>> = centroids
        .iter()
        .map(|c| c.iter().map(|f| *f as f64).collect())
        .collect();
    let mut per_cid: Vec<Vec<Posting>> = vec![Vec::new(); centroids.len()];
    for (i, v) in domain.iter().enumerate() {
        let (cid, _) = nearest(&centroids_f64, v);
        per_cid[cid].push(Posting {
            pk: raws[i].0.clone(),
            sq8: quant::encode(&cal, v),
        });
    }
    let table_id = schema.id;
    let colp = col_pos as u32;
    let mut out = vec![
        (
            crate::sql::index::vector::meta_key(table_id, colp),
            Some(encode_meta(params, per_cid.len(), &cal)),
        ),
        (
            crate::sql::index::vector::centroid_key(table_id, colp),
            Some(encode_centroids(&centroids)),
        ),
    ];
    for (cid, entries) in per_cid.into_iter().enumerate() {
        out.push((
            posting_key(table_id, colp, cid as u32),
            Some(encode_postings(&entries)),
        ));
    }
    Ok(out)
}

/// Drop every physical record of one vector index (paged, kind-filtered
/// walk of the index's slot -- vector kinds never collide with the
/// 0x21/0x22 parser, so this sweep is self-contained).
pub fn drop_ops(store: &Store, table_id: u32, col_pos: u32) -> Result<Vec<IndexOp>, String> {
    let start = sweep_start(table_id, col_pos);
    let end = crate::store::rocksdb::key_upper_bound(&start).unwrap_or_default();
    let mut dead: Vec<IndexOp> = Vec::new();
    ops::for_each_from(store, &start, false, &mut |key, _| {
        if !end.is_empty() && key >= end.as_slice() {
            return false;
        }
        // after "<slot/>" comes the kind byte
        if key.len() > start.len() && is_vector_kind(key[start.len()]) {
            dead.push((key.to_vec(), None));
        }
        dead.len() < 4096
    })?;
    Ok(dead)
}

/// Posting-table state of one index, for assignment and RMW: centroids
/// in f64, meta params, and the live calibration.
pub struct Loaded {
    pub params: VectorIndexParams,
    pub centroids: Vec<Vec<f64>>,
    pub cal: Calibration,
}

pub fn load(store: &Store, table_id: u32, col_pos: u32) -> Result<Option<Loaded>, String> {
    let meta = ops::get_physical(
        store,
        &crate::sql::index::vector::meta_key(table_id, col_pos),
    )?;
    let Some(meta) = meta else { return Ok(None) };
    let (params, k, cal) =
        decode_meta(&meta).ok_or_else(|| "corrupt vector index meta".to_string())?;
    let blob = ops::get_physical(
        store,
        &crate::sql::index::vector::centroid_key(table_id, col_pos),
    )?
    .ok_or_else(|| "vector index meta without centroids".to_string())?;
    let cs = decode_centroids(&blob, params.dim as usize)
        .ok_or_else(|| "corrupt vector index centroids".to_string())?;
    if cs.len() != k {
        return Err("vector index centroid count drift".into());
    }
    Ok(Some(Loaded {
        params,
        centroids: cs
            .into_iter()
            .map(|c| c.into_iter().map(|f| f as f64).collect())
            .collect(),
        cal,
    }))
}

/// Derived posting ops of one batch's row transitions for one vector
/// index: remove the old assignment, add the new. NULL/absent vectors
/// only remove. The overlay keeps partition reads consistent within
/// the batch even when several transitions touch one partition.
pub fn transition_ops(
    store: &Store,
    schema: &TableSchema,
    col_pos: usize,
    loaded: &Loaded,
    transitions: &[Transition<'_>],
) -> Result<Vec<IndexOp>, String> {
    let dim = loaded.params.dim as usize;
    let mut overlay: BTreeMap<Vec<u8>, Vec<Posting>> = BTreeMap::new();
    let mut changed: Vec<Vec<u8>> = Vec::new();
    let mut mutate = |store: &Store,
                      overlay: &mut BTreeMap<Vec<u8>, Vec<Posting>>,
                      cid: usize,
                      f: &dyn Fn(&mut Vec<Posting>)|
     -> Result<(), String> {
        let key = posting_key(schema.id, col_pos as u32, cid as u32);
        let entries = overlay.entry(key.clone()).or_insert_with(|| {
            ops::get_physical(store, &key)
                .ok()
                .flatten()
                .and_then(|v| decode_postings(&v, dim))
                .unwrap_or_default()
        });
        f(entries);
        if !changed.contains(&key) {
            changed.push(key);
        }
        Ok(())
    };
    for t in transitions {
        if let Some((old_pk, old_vec)) = side_vector(t.old, col_pos) {
            let cid = nearest(
                &loaded.centroids,
                &to_domain(&old_vec, loaded.params.metric),
            )
            .0;
            let pk = old_pk.to_vec();
            mutate(store, &mut overlay, cid, &|entries| {
                entries.retain(|e| e.pk != pk);
            })?;
        }
        if let Some((new_pk, new_vec)) = side_vector(t.new, col_pos) {
            let v = to_domain(&new_vec, loaded.params.metric);
            let (cid, _) = nearest(&loaded.centroids, &v);
            let pk = new_pk.to_vec();
            let sq8 = quant::encode(&loaded.cal, &v);
            let entry = Posting { pk, sq8 };
            mutate(store, &mut overlay, cid, &|entries| match entries
                .binary_search_by(|e| e.pk.cmp(&entry.pk))
            {
                Ok(at) => entries[at].sq8 = entry.sq8.clone(),
                Err(at) => entries.insert(
                    at,
                    Posting {
                        pk: entry.pk.clone(),
                        sq8: entry.sq8.clone(),
                    },
                ),
            })?;
        }
    }
    let mut out = Vec::with_capacity(changed.len());
    for key in changed {
        let entries = overlay.remove(&key).expect("changed keys hold entries");
        out.push((key, Some(encode_postings(&entries))));
    }
    Ok(out)
}

/// Old side of a transition as (pk, vector) when it holds one.
fn side_vector(side: Option<RowSide<'_>>, col_pos: usize) -> Option<(&[u8], Vec<f32>)> {
    let s = side?;
    match &s.values.get(col_pos)? {
        Value::Vector(xs) => Some((s.pk_key, xs.clone())),
        _ => None,
    }
}

/// The k-means/SQ8 view of one vector (cosine normalizes; zero vectors
/// stay zero -- they simply cluster near the origin).
pub fn to_domain(xs: &[f32], metric: Metric) -> Vec<f64> {
    let v: Vec<f64> = xs.iter().map(|f| *f as f64).collect();
    if metric == Metric::Cosine {
        let n = crate::search::vecmath::norm(&v);
        if n > 0.0 {
            return v.iter().map(|x| x / n).collect();
        }
    }
    v
}

/// The declared dim of a VECTOR column, if `col_pos` is one.
pub fn column_dim(schema: &TableSchema, col_pos: usize) -> Option<u32> {
    match schema.column(col_pos).sql_type {
        SqlType::Vector { dim } => Some(dim),
        _ => None,
    }
}
