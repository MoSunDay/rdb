//! Inverted-index write/read path: the ONE read-modify-write layer
//! behind FT.ADD/FT.DEL/FT.SEARCH. Every mutation lands in a single
//! fsync-ed WriteBatch (posting records for all touched terms, the
//! doc record, term stats and the meta counters together) so a crash
//! never leaves a posting without its doc or vice versa.
//!
//! Callers hold the index-key latch across build+commit (the command
//! layer's job, same contract as every typed family).

use rocksdb::WriteBatch;

use crate::ds::codec::{encode_envelope, SEARCH_FAMILY};
use crate::ds::expire;
use crate::store::{ops, Store};

use super::ann;
use super::index_codec::{
    decode_doc, decode_meta, decode_posting, decode_termstat, encode_doc, encode_meta,
    encode_posting, encode_termstat, meta_key, posting_key, remove_posting, termstat_key,
    upsert_posting, DocRecord, PostEntry, TermStat,
};

pub use super::index_codec::{FieldType, IndexField, IndexMeta};

/// Decode the meta payload the command layer's `keys_core::resolve`
/// just handed over (wrong-type detection lives there, like every
/// typed family).
pub fn decode_index_meta(expire_ms: u64, payload: &[u8]) -> Result<IndexMeta, String> {
    decode_meta(payload)
        .map(|mut meta| {
            meta.expire_ms = expire_ms;
            meta
        })
        .ok_or_else(|| "ERR corrupt index meta".to_string())
}

/// Meta record write keeping the TTL envelope + expire index entry in
/// sync (unchanged expiry re-asserts the entry, the `write_meta`
/// pattern of the other families).
pub fn put_meta(batch: &mut WriteBatch, prefix: &[u8], index: &[u8], meta: &IndexMeta) {
    let root = meta_key(prefix, index);
    batch.put(&root, encode_envelope(meta.expire_ms, &encode_meta(meta)));
    expire::set_ttl_entries(batch, prefix, root, meta.expire_ms, meta.expire_ms);
}

pub fn read_doc(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    docid: &[u8],
) -> Result<Option<DocRecord>, String> {
    let key = super::index_codec::doc_key(prefix, index, docid);
    match ops::get_physical(store, &key)? {
        None => Ok(None),
        Some(v) => decode_doc(&v)
            .map(Some)
            .ok_or_else(|| "corrupt doc record".to_string()),
    }
}

/// One term's posting list (empty when the term is unseen).
pub fn read_posting(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    term: &[u8],
) -> Result<Vec<PostEntry>, String> {
    let key = posting_key(prefix, index, field, term);
    match ops::get_physical(store, &key)? {
        None => Ok(Vec::new()),
        Some(v) => decode_posting(&v).ok_or_else(|| "corrupt posting".to_string()),
    }
}

/// df/total_tf of one term (zeros when unseen).
pub fn read_termstat(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    term: &[u8],
) -> Result<TermStat, String> {
    let key = termstat_key(prefix, index, field, term);
    match ops::get_physical(store, &key)? {
        None => Ok(TermStat { df: 0, total_tf: 0 }),
        Some(v) => decode_termstat(&v).ok_or_else(|| "corrupt termstat".to_string()),
    }
}

/// Build the single batch that adds-or-replaces `docid` with `rec`
/// (terms + optional vector + raw doc bytes). `meta` is the CURRENT
/// meta (counters/statistics are mutated into the returned batch).
/// Vector partitioning consults the ANN centroid table when the schema
/// has a VECTOR field and a table exists (see `ann`).
pub fn build_add_batch(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    docid: &[u8],
    mut rec: DocRecord,
) -> Result<WriteBatch, String> {
    let old = read_doc(store, prefix, index, docid)?;
    let mut batch = WriteBatch::default();

    // -- postings: deltas of new-vs-old per (field, term) --
    let mut deltas: Vec<(Vec<u8>, Vec<u8>, i64, i64)> = Vec::new();
    let bump = |deltas: &mut Vec<(Vec<u8>, Vec<u8>, i64, i64)>,
                field: &[u8],
                term: &[u8],
                tf: i64,
                df: i64| {
        for d in deltas.iter_mut() {
            if d.0 == field && d.1 == term {
                d.2 += tf;
                d.3 += df;
                return;
            }
        }
        deltas.push((field.to_vec(), term.to_vec(), tf, df));
    };
    for t in &rec.terms {
        bump(&mut deltas, &t.field, &t.term, t.tf as i64, 1);
    }
    if let Some(old) = &old {
        for t in &old.terms {
            bump(&mut deltas, &t.field, &t.term, -(t.tf as i64), -1);
        }
    }
    for (field, term, tf_delta, df_delta) in deltas {
        let old_tf = old
            .as_ref()
            .and_then(|o| {
                o.terms
                    .iter()
                    .find(|t| t.field == field && t.term == term)
                    .map(|t| t.tf)
            })
            .unwrap_or(0);
        let mut entries = read_posting(store, prefix, index, &field, &term)?;
        upsert_by_delta(&mut entries, docid, tf_delta, old_tf);
        let mut stat = read_termstat(store, prefix, index, &field, &term)?;
        stat.df = (stat.df as i64 + df_delta).max(0) as u64;
        stat.total_tf = (stat.total_tf as i64 + tf_delta).max(0) as u64;
        let pkey = posting_key(prefix, index, &field, &term);
        let tkey = termstat_key(prefix, index, &field, &term);
        if entries.is_empty() {
            batch.delete(pkey);
            batch.delete(tkey);
        } else {
            batch.put(pkey, encode_posting(&entries));
            batch.put(tkey, encode_termstat(&stat));
        }
    }

    // -- vector: leave the old partition, join the new one --
    if let Some((field, dim)) = vector_field(meta) {
        if let Some(old) = &old {
            if old.centroid != super::index_codec::NO_CENTROID && !old.vector.is_empty() {
                ann::partition_remove(
                    &mut batch,
                    store,
                    prefix,
                    index,
                    &field,
                    dim,
                    old.centroid,
                    docid,
                )?;
            }
        }
        if rec.vector.len() == dim as usize {
            match ann::assign(store, prefix, index, field.as_slice(), dim, &rec.vector)? {
                Some(cid) => {
                    ann::partition_add(
                        &mut batch,
                        store,
                        prefix,
                        index,
                        &field,
                        dim,
                        cid,
                        docid,
                        &rec.vector,
                    )?;
                    rec.centroid = cid;
                }
                None => rec.centroid = super::index_codec::NO_CENTROID,
            }
        } else if !rec.vector.is_empty() {
            return Err("ERR vector dimension mismatch".to_string());
        } else {
            rec.centroid = super::index_codec::NO_CENTROID;
        }
    }

    // -- meta counters + the doc record itself --
    let mut new_meta = meta.clone();
    if old.is_none() {
        new_meta.num_docs += 1;
    }
    new_meta.sum_doclen =
        (new_meta.sum_doclen + rec.doclen).wrapping_sub(old.as_ref().map_or(0, |o| o.doclen));
    put_meta(&mut batch, prefix, index, &new_meta);
    batch.put(
        super::index_codec::doc_key(prefix, index, docid),
        encode_doc(&rec),
    );
    Ok(batch)
}

/// Set/remove docid in a posting list given the tf delta vs the OLD tf.
fn upsert_by_delta(entries: &mut Vec<PostEntry>, docid: &[u8], tf_delta: i64, old_tf: u64) -> bool {
    let new_tf = (old_tf as i64 + tf_delta).max(0) as u64;
    if new_tf == 0 {
        return remove_posting(entries, docid);
    }
    upsert_posting(entries, docid, new_tf)
}

/// Build the batch deleting `docid`; `Ok(None)` when the doc is absent.
pub fn build_del_batch(
    store: &Store,
    prefix: &[u8],
    index: &[u8],
    meta: &IndexMeta,
    docid: &[u8],
) -> Result<Option<WriteBatch>, String> {
    let Some(old) = read_doc(store, prefix, index, docid)? else {
        return Ok(None);
    };
    let mut batch = WriteBatch::default();
    for t in &old.terms {
        let mut entries = read_posting(store, prefix, index, &t.field, &t.term)?;
        remove_posting(&mut entries, docid);
        let mut stat = read_termstat(store, prefix, index, &t.field, &t.term)?;
        stat.df = stat.df.saturating_sub(1);
        stat.total_tf = stat.total_tf.saturating_sub(t.tf);
        let pkey = posting_key(prefix, index, &t.field, &t.term);
        let tkey = termstat_key(prefix, index, &t.field, &t.term);
        if entries.is_empty() {
            batch.delete(pkey);
            batch.delete(tkey);
        } else {
            batch.put(pkey, encode_posting(&entries));
            batch.put(tkey, encode_termstat(&stat));
        }
    }
    if let Some((field, dim)) = vector_field(meta) {
        if old.centroid != super::index_codec::NO_CENTROID && !old.vector.is_empty() {
            ann::partition_remove(
                &mut batch,
                store,
                prefix,
                index,
                &field,
                dim,
                old.centroid,
                docid,
            )?;
        }
    }
    let mut new_meta = meta.clone();
    new_meta.num_docs = new_meta.num_docs.saturating_sub(1);
    new_meta.sum_doclen = new_meta.sum_doclen.saturating_sub(old.doclen);
    put_meta(&mut batch, prefix, index, &new_meta);
    batch.delete(super::index_codec::doc_key(prefix, index, docid));
    Ok(Some(batch))
}

/// The schema's single VECTOR field (name, dim); search engine v1
/// supports one vector field per index.
pub fn vector_field(meta: &IndexMeta) -> Option<(Vec<u8>, u64)> {
    meta.fields.iter().find_map(|f| match &f.ftype {
        FieldType::Vector { dim } => Some((f.name.clone(), *dim)),
        FieldType::Text => None,
    })
}

/// TEXT field names (query-side scope defaults).
pub fn text_fields(meta: &IndexMeta) -> Vec<Vec<u8>> {
    meta.fields
        .iter()
        .filter(|f| matches!(f.ftype, FieldType::Text))
        .map(|f| f.name.clone())
        .collect()
}

/// Wipe the whole family (FT.DROP / lazy purge path); the family span
/// covers text AND ANN records plus the TTL index entry.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], index: &[u8], expire_ms: u64) {
    super::index_codec::delete_family_entries(batch, prefix, index, expire_ms);
    let _ = SEARCH_FAMILY; // span asserted by codec::family_delete_ranges
}
