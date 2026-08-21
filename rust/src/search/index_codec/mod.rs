//! Physical codecs for the search-engine record family (kinds
//! 0x13..=0x18, `codec::SEARCH_FAMILY`). One family on purpose: the
//! index meta's TTL envelope drives a lazy purge that must wipe text
//! AND vector side-records together.
//!
//! Layouts (on top of `ds::codec` derived keys, LEB128 counts):
//! ```text
//! meta value     = n_docs ++ sum_doclen ++ nfields
//!                  ++ per field [ name_len++name ++ type:u8 ++ dim? ]
//! doc suffix     = docid_len ++ docid
//! doc value      = doclen ++ nentries
//!                  ++ per entry [ field++term++tf ]      (postings undo log)
//!                  ++ centroid:u64 BE                    (u64::MAX = none)
//!                  ++ vec_len ++ raw LE f64s             (exact rerank)
//!                  ++ raw JSON bytes                     (content reply)
//! posting sfx    = field_len++field ++ term_len++term
//! posting value  = count ++ entries sorted by docid [ docid ++ tf ]
//! termstat value = df ++ total_tf
//! centroid sfx   = field_len ++ field
//! centroid value = k ++ dim ++ k*dim*f32LE ++ dim*min ++ dim*scale
//!                  ++ k member counts                    (SQ8 calibration)
//! annpost sfx    = field_len++field ++ centroid_id
//! annpost value  = dim ++ entries sorted by docid [ docid ++ dim SQ8 bytes ]
//! ```
//! SQ8 calibration (per-dimension min/scale) is GLOBAL per field and
//! rides in the centroid record, so one quantized entry costs ~1 byte
//! per dimension; out-of-range values clamp to [0,255] on encode.

use crate::ds::codec::{
    data_key, elem_key, encode_count, family_delete_ranges, KIND_ANN_CENTROID, KIND_ANN_POSTING,
    KIND_SEARCH_DOC, KIND_SEARCH_META, KIND_SEARCH_POSTING, KIND_SEARCH_TERMSTAT, SEARCH_FAMILY,
};
use crate::store::key_upper_bound;

pub mod posting;

pub use posting::{
    decode_centroids, decode_posting, decode_termstat, encode_centroids, encode_posting,
    encode_termstat, remove_posting, upsert_posting, CentroidTable, PostEntry, TermStat,
};

/// `u64::MAX` sentinel: a vector doc not yet assigned to any ANN
/// partition (no centroid table, or added before FT.BUILD).
pub const NO_CENTROID: u64 = u64::MAX;

/// Field type byte in the meta schema.
pub const FIELD_TEXT: u8 = 1;
pub const FIELD_VECTOR: u8 = 2;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Text,
    Vector { dim: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexField {
    pub name: Vec<u8>,
    pub ftype: FieldType,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IndexMeta {
    pub expire_ms: u64,
    pub num_docs: u64,
    pub sum_doclen: u64,
    pub fields: Vec<IndexField>,
}

/// One (field, term, tf) triple of a doc -- exactly what must be
/// subtracted from postings when the doc is deleted or replaced.
#[derive(Debug, Clone, PartialEq)]
pub struct TermFreq {
    pub field: Vec<u8>,
    pub term: Vec<u8>,
    pub tf: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DocRecord {
    pub doclen: u64,
    pub terms: Vec<TermFreq>,
    pub centroid: u64,
    pub vector: Vec<f64>,
    pub doc: Vec<u8>,
}

/// Physical root of the index meta record.
pub fn meta_key(prefix: &[u8], index: &[u8]) -> Vec<u8> {
    data_key(prefix, KIND_SEARCH_META, index)
}

pub fn encode_meta(meta: &IndexMeta) -> Vec<u8> {
    let mut out = encode_count(meta.num_docs);
    out.extend_from_slice(&encode_count(meta.sum_doclen));
    out.extend_from_slice(&encode_count(meta.fields.len() as u64));
    for f in &meta.fields {
        out.extend_from_slice(&encode_count(f.name.len() as u64));
        out.extend_from_slice(&f.name);
        match f.ftype {
            FieldType::Text => out.push(FIELD_TEXT),
            FieldType::Vector { dim } => {
                out.push(FIELD_VECTOR);
                out.extend_from_slice(&encode_count(dim));
            }
        }
    }
    out
}

/// `None` on malformed payloads (truncated / bad type byte).
pub fn decode_meta(payload: &[u8]) -> Option<IndexMeta> {
    let (num_docs, rest) = take_count(payload)?;
    let (sum_doclen, rest) = take_count(rest)?;
    let (nfields, mut rest) = take_count(rest)?;
    let mut fields = Vec::with_capacity(nfields.min(1024) as usize);
    for _ in 0..nfields {
        let (flen, r) = take_count(rest)?;
        let name = r.get(..flen as usize)?.to_vec();
        rest = r.get(flen as usize..)?;
        let ftype = *rest.first()?;
        rest = &rest[1..];
        let ftype = match ftype {
            FIELD_TEXT => FieldType::Text,
            FIELD_VECTOR => {
                let (dim, r) = take_count(rest)?;
                rest = r;
                FieldType::Vector { dim }
            }
            _ => return None,
        };
        fields.push(IndexField { name, ftype });
    }
    Some(IndexMeta {
        expire_ms: 0,
        num_docs,
        sum_doclen,
        fields,
    })
}

pub fn doc_key(prefix: &[u8], index: &[u8], docid: &[u8]) -> Vec<u8> {
    let mut suffix = encode_count(docid.len() as u64);
    suffix.extend_from_slice(docid);
    elem_key(prefix, KIND_SEARCH_DOC, index, &suffix)
}

pub fn encode_doc(rec: &DocRecord) -> Vec<u8> {
    let mut out = encode_count(rec.doclen);
    out.extend_from_slice(&encode_count(rec.terms.len() as u64));
    for t in &rec.terms {
        out.extend_from_slice(&encode_count(t.field.len() as u64));
        out.extend_from_slice(&t.field);
        out.extend_from_slice(&encode_count(t.term.len() as u64));
        out.extend_from_slice(&t.term);
        out.extend_from_slice(&encode_count(t.tf));
    }
    out.extend_from_slice(&rec.centroid.to_be_bytes());
    out.extend_from_slice(&encode_count(rec.vector.len() as u64));
    for &x in &rec.vector {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.extend_from_slice(&rec.doc);
    out
}

pub fn decode_doc(value: &[u8]) -> Option<DocRecord> {
    let (doclen, rest) = take_count(value)?;
    let (n, mut rest) = take_count(rest)?;
    let mut terms = Vec::with_capacity(n.min(4096) as usize);
    for _ in 0..n {
        let (flen, r) = take_count(rest)?;
        let field = r.get(..flen as usize)?.to_vec();
        let r = r.get(flen as usize..)?;
        let (tlen, r) = take_count(r)?;
        let term = r.get(..tlen as usize)?.to_vec();
        let r = r.get(tlen as usize..)?;
        let (tf, r) = take_count(r)?;
        rest = r;
        terms.push(TermFreq { field, term, tf });
    }
    let centroid = u64::from_be_bytes(rest.get(..8)?.try_into().ok()?);
    let rest = &rest[8..];
    let (vlen, rest) = take_count(rest)?;
    let mut vector = Vec::with_capacity(vlen.min(65536) as usize);
    let raw = rest.get(..vlen as usize * 8)?;
    let rest = &rest[vlen as usize * 8..];
    for chunk in raw.chunks_exact(8) {
        vector.push(f64::from_le_bytes(chunk.try_into().ok()?));
    }
    Some(DocRecord {
        doclen,
        terms,
        centroid,
        vector,
        doc: rest.to_vec(),
    })
}

/// Shared (field, term) suffix of posting and termstat keys; term
/// order inside the key is bytewise, matching posting iteration.
pub fn term_suffix(field: &[u8], term: &[u8]) -> Vec<u8> {
    let mut out = encode_count(field.len() as u64);
    out.extend_from_slice(field);
    out.extend_from_slice(&encode_count(term.len() as u64));
    out.extend_from_slice(term);
    out
}

pub fn posting_key(prefix: &[u8], index: &[u8], field: &[u8], term: &[u8]) -> Vec<u8> {
    elem_key(
        prefix,
        KIND_SEARCH_POSTING,
        index,
        &term_suffix(field, term),
    )
}

pub fn termstat_key(prefix: &[u8], index: &[u8], field: &[u8], term: &[u8]) -> Vec<u8> {
    elem_key(
        prefix,
        KIND_SEARCH_TERMSTAT,
        index,
        &term_suffix(field, term),
    )
}

/// `f(term_suffix) -> bool` continuation over one index's term keys.
pub fn termstat_range(prefix: &[u8], index: &[u8]) -> (Vec<u8>, Vec<u8>) {
    kind_range(prefix, KIND_SEARCH_TERMSTAT, index)
}

pub fn centroid_key(prefix: &[u8], index: &[u8], field: &[u8]) -> Vec<u8> {
    let mut suffix = encode_count(field.len() as u64);
    suffix.extend_from_slice(field);
    elem_key(prefix, KIND_ANN_CENTROID, index, &suffix)
}

pub fn ann_posting_key(prefix: &[u8], index: &[u8], field: &[u8], centroid: u64) -> Vec<u8> {
    let mut suffix = encode_count(field.len() as u64);
    suffix.extend_from_slice(field);
    suffix.extend_from_slice(&encode_count(centroid));
    elem_key(prefix, KIND_ANN_POSTING, index, &suffix)
}

/// `[lower, upper)` span of one index's ANN partitions for `field`
/// (field prefix encoded once; the +1-length trick cannot cross into
/// another field because the LEB128 length grows before the bytes).
pub fn ann_posting_range(prefix: &[u8], index: &[u8], field: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = encode_count(field.len() as u64);
    suffix.extend_from_slice(field);
    let lower = elem_key(prefix, KIND_ANN_POSTING, index, &suffix);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
}

/// `[lower, upper)` span of one index's docs.
pub fn doc_range(prefix: &[u8], index: &[u8]) -> (Vec<u8>, Vec<u8>) {
    kind_range(prefix, KIND_SEARCH_DOC, index)
}

/// `[lower, upper)` span of every record of `kind` under `index`
/// (empty upper = to the end of the keyspace, `key_upper_bound`'s
/// all-0xff convention).
fn kind_range(prefix: &[u8], kind: u8, index: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lower = data_key(prefix, kind, index);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
}

/// Batch entries wiping the whole search family (all six kinds) and
/// the TTL index entry; used by FT.DROP and lazy purge.
pub fn delete_family_entries(
    batch: &mut rocksdb::WriteBatch,
    prefix: &[u8],
    index: &[u8],
    expire_ms: u64,
) {
    for (lower, upper) in family_delete_ranges(prefix, SEARCH_FAMILY, index) {
        batch.delete_range(lower, upper);
    }
    if expire_ms > 0 {
        batch.delete(crate::ds::codec::expire_index_key(
            prefix,
            expire_ms,
            &meta_key(prefix, index),
        ));
    }
}

fn take_count(payload: &[u8]) -> Option<(u64, &[u8])> {
    let mut v = 0u64;
    for (i, &b) in payload.iter().enumerate() {
        if i < 9 {
            v |= u64::from(b & 0x7f) << (7 * i);
        }
        if b & 0x80 == 0 {
            return Some((v, &payload[i + 1..]));
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrip() {
        let meta = IndexMeta {
            expire_ms: 0,
            num_docs: 7,
            sum_doclen: 12345,
            fields: vec![
                IndexField {
                    name: b"title".to_vec(),
                    ftype: FieldType::Text,
                },
                IndexField {
                    name: b"v".to_vec(),
                    ftype: FieldType::Vector { dim: 4096 },
                },
            ],
        };
        assert_eq!(decode_meta(&encode_meta(&meta)).unwrap(), meta);
        assert!(decode_meta(b"\xff").is_none());
    }

    #[test]
    fn doc_roundtrip() {
        let rec = DocRecord {
            doclen: 42,
            terms: vec![TermFreq {
                field: b"body".to_vec(),
                term: "搜索".as_bytes().to_vec(),
                tf: 3,
            }],
            centroid: NO_CENTROID,
            vector: vec![0.25, -1.5],
            doc: br#"{"a":1}"#.to_vec(),
        };
        assert_eq!(decode_doc(&encode_doc(&rec)).unwrap(), rec);
        assert!(decode_doc(&[0x7f]).is_none());
    }
}
