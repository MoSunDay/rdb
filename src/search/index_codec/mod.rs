//! Physical codecs for the search-engine record family (kinds
//! 0x13..=0x1A, `codec::SEARCH_FAMILY`). One family on purpose: the
//! index meta's TTL envelope drives a lazy purge that must wipe text
//! AND vector side-records together.
//!
//! Layouts (on top of `ds::codec` derived keys, LEB128 counts):
//! ```text
//! meta value     = n_docs ++ sum_doclen ++ nfields
//!                  ++ per field [ name_len++name ++ type:u8 ++ dim? ]
//!                  (type bytes: 1 TEXT, 2 VECTOR(+dim), 3 KEYWORD,
//!                   4 NUMERIC -- keyword/numeric carry no payload)
//!                  ++ n_stopwords ++ per stopword [ len++bytes ]
//!                  ++ k1:f64LE ++ b:f64LE        (v2 append-only tail;
//!                  payload ending at the fields = pre-v2 meta decoded
//!                  with default empty stopwords and k1/b = 1.2/0.75)
//! doc suffix     = docid_len ++ docid
//! doc value      = doclen ++ nentries
//!                  ++ per entry [ field++term++tf ]      (postings undo log)
//!                  ++ centroid:u64 BE                    (u64::MAX = none)
//!                  ++ vec_len ++ raw LE f64s             (exact rerank)
//!                  ++ raw JSON bytes                     (content reply)
//! posting sfx    = field_len++field ++ term_len++term
//! posting value  = count ++ entries sorted by docid [ docid ++ tf ]      (v1, kind 0x15)
//! posting2 value = count ++ entries sorted by docid
//!                  [ docid ++ tf ++ npos ++ Δpositions ]  (v2, kind 0x1A;
//!                  positions = strictly increasing token ordinals,
//!                  delta-encoded as LEB128(pos[i]-pos[i-1]-1), first
//!                  delta is pos[0]; npos=0 = unknown, the v1 fallback)
//! termstat value = df ++ total_tf
//! centroid sfx   = field_len ++ field
//! centroid value = k ++ dim ++ k*dim*f32LE ++ dim*min ++ dim*scale
//!                  ++ k member counts                    (SQ8 calibration)
//! annpost sfx    = field_len++field ++ centroid_id
//! annpost value  = dim ++ entries sorted by docid [ docid ++ dim SQ8 bytes ]
//! numval sfx     = field_len++field ++ docid            (kind 0x19)
//! numval value   = f64 LE                                (NUMERIC doc value)
//! ```
//! SQ8 calibration (per-dimension min/scale) is GLOBAL per field and
//! rides in the centroid record, so one quantized entry costs ~1 byte
//! per dimension; out-of-range values clamp to [0,255] on encode.

use crate::ds::codec::{
    data_key, elem_key, encode_count, family_delete_ranges, KIND_ANN_CENTROID, KIND_ANN_POSTING,
    KIND_SEARCH_DOC, KIND_SEARCH_META, KIND_SEARCH_POSTING, KIND_SEARCH_POSTING2,
    KIND_SEARCH_TERMSTAT, SEARCH_FAMILY,
};
use crate::store::key_upper_bound;

pub mod numval;
pub mod posting;

pub use numval::{decode_numval, encode_numval, numval_key, numval_range};
pub use posting::{
    decode_centroids, decode_posting, decode_posting2, decode_termstat, encode_centroids,
    encode_posting, encode_posting2, encode_termstat, remove_posting, upsert_posting,
    CentroidTable, PostEntry, TermStat,
};

/// `u64::MAX` sentinel: a vector doc not yet assigned to any ANN
/// partition (no centroid table, or added before FT.BUILD).
pub const NO_CENTROID: u64 = u64::MAX;

/// Field type byte in the meta schema.
pub const FIELD_TEXT: u8 = 1;
pub const FIELD_VECTOR: u8 = 2;
pub const FIELD_KEYWORD: u8 = 3;
pub const FIELD_NUMERIC: u8 = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Text,
    Vector {
        dim: u64,
    },
    /// Exact-match, untokenized single terms (one posting entry each,
    /// tf folds duplicates).
    Keyword,
    /// Columnar f64 doc value in a kind 0x19 record (no postings).
    Numeric,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexField {
    pub name: Vec<u8>,
    pub ftype: FieldType,
}

/// Index-level tuning: BM25 `k1`/`b` and the stopword list (both the
/// index and the query side read them). Manual `Default`: f64's zero
/// default is meaningless for k1/b, so they fall back to the `bm25`
/// constants (the pre-tail payload decodes to exactly this).
#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    pub expire_ms: u64,
    pub num_docs: u64,
    pub sum_doclen: u64,
    pub fields: Vec<IndexField>,
    pub stopwords: Vec<Vec<u8>>,
    pub k1: f64,
    pub b: f64,
}

impl Default for IndexMeta {
    fn default() -> Self {
        IndexMeta {
            expire_ms: 0,
            num_docs: 0,
            sum_doclen: 0,
            fields: Vec::new(),
            stopwords: Vec::new(),
            k1: super::bm25::K1,
            b: super::bm25::B,
        }
    }
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
            FieldType::Keyword => out.push(FIELD_KEYWORD),
            FieldType::Numeric => out.push(FIELD_NUMERIC),
        }
    }
    // append-only v2 tail: stopwords + BM25 knobs (f64 LE)
    out.extend_from_slice(&encode_count(meta.stopwords.len() as u64));
    for s in &meta.stopwords {
        out.extend_from_slice(&encode_count(s.len() as u64));
        out.extend_from_slice(s);
    }
    out.extend_from_slice(&meta.k1.to_le_bytes());
    out.extend_from_slice(&meta.b.to_le_bytes());
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
            FIELD_KEYWORD => FieldType::Keyword,
            FIELD_NUMERIC => FieldType::Numeric,
            _ => return None,
        };
        fields.push(IndexField { name, ftype });
    }
    // tail absent = pre-v2 payload: defaults (empty stopwords, the
    // bm25 constants); any truncation inside the tail is malformed.
    if rest.is_empty() {
        return Some(IndexMeta {
            expire_ms: 0,
            num_docs,
            sum_doclen,
            fields,
            ..Default::default()
        });
    }
    let (nstop, mut rest) = take_count(rest)?;
    let mut stopwords = Vec::with_capacity(nstop.min(10000) as usize);
    for _ in 0..nstop {
        let (slen, r) = take_count(rest)?;
        let s = r.get(..slen as usize)?.to_vec();
        rest = r.get(slen as usize..)?;
        stopwords.push(s);
    }
    let k1 = f64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
    let b = f64::from_le_bytes(rest.get(8..16)?.try_into().ok()?);
    Some(IndexMeta {
        expire_ms: 0,
        num_docs,
        sum_doclen,
        fields,
        stopwords,
        k1,
        b,
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
    for chunk in raw.as_chunks::<8>().0 {
        vector.push(f64::from_le_bytes(*chunk));
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

/// v2 posting key (kind 0x1A): same `term_suffix` as v1 so the two
/// records of one term differ only by kind byte.
pub fn posting2_key(prefix: &[u8], index: &[u8], field: &[u8], term: &[u8]) -> Vec<u8> {
    elem_key(
        prefix,
        KIND_SEARCH_POSTING2,
        index,
        &term_suffix(field, term),
    )
}

/// Inverse of [`term_suffix`] -> `(field, term)`; `None` on truncated
/// lengths. Used by prefix scans that decode the suffix of every
/// posting key a range walk hands over.
pub fn decode_term_suffix(suffix: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (flen, rest) = take_count(suffix)?;
    let field = rest.get(..flen as usize)?.to_vec();
    let rest = rest.get(flen as usize..)?;
    let (tlen, rest) = take_count(rest)?;
    let term = rest.get(..tlen as usize)?.to_vec();
    Some((field, term))
}

/// `[lower, upper)` span of one field's postings of `kind` (0x15 v1
/// or 0x1A v2): lower anchors on `field_len ++ field` and upper is
/// its last-byte increment -- the `ann_posting_range` pattern (the
/// +1-length trick cannot cross into another field because the LEB128
/// length grows before the bytes). The range covers the WHOLE field;
/// callers filter terms (e.g. a term prefix) in the callback.
pub fn posting_field_range(
    prefix: &[u8],
    index: &[u8],
    field: &[u8],
    kind: u8,
) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = encode_count(field.len() as u64);
    suffix.extend_from_slice(field);
    let lower = elem_key(prefix, kind, index, &suffix);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
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

/// Batch entries wiping the whole search family (all seven kinds) and
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
            ..Default::default()
        };
        assert_eq!(decode_meta(&encode_meta(&meta)).unwrap(), meta);
        assert!(decode_meta(b"\xff").is_none());
    }

    #[test]
    fn meta_tail_roundtrip_and_pre_v2_default() {
        let meta = IndexMeta {
            num_docs: 3,
            sum_doclen: 9,
            stopwords: vec![b"the".to_vec(), "的".as_bytes().to_vec()],
            k1: 1.5,
            b: 0.5,
            ..Default::default()
        };
        let full = encode_meta(&meta);
        assert_eq!(decode_meta(&full).unwrap(), meta);

        // pre-v2 payload (truncated right after the fields): the
        // defaults -- empty stopwords, k1=1.2, b=0.75.
        let count_len = |v: u64| crate::ds::codec::encode_count(v).len();
        let mut tail = count_len(meta.stopwords.len() as u64);
        for s in &meta.stopwords {
            tail += count_len(s.len() as u64) + s.len();
        }
        tail += 16; // k1 + b
        let old = &full[..full.len() - tail];
        let decoded = decode_meta(old).unwrap();
        assert_eq!(decoded.num_docs, 3);
        assert!(decoded.stopwords.is_empty());
        assert!((decoded.k1 - 1.2).abs() < 1e-12);
        assert!((decoded.b - 0.75).abs() < 1e-12);

        // truncated inside the tail -> malformed, not defaults
        assert!(decode_meta(&full[..full.len() - 8]).is_none());
        assert!(decode_meta(&full[..old.len() + 1]).is_none());
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
