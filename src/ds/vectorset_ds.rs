//! Vector-set storage ops (name `vectorset_ds` mirrors `zset_ds`/
//! `json_ds`; the P4 last of the seven structures). Physical layout,
//! ONE meta + N elem records per user key:
//!
//! ```text
//! meta = data_key(prefix, KIND_VECTORSET_META, key)
//!        value = envelope(LEB128 expire_ms) ++ LEB128(dim) ++ LEB128(count)
//! elem = elem_key(prefix, KIND_VECTORSET_ELEM, key, element)
//!        value = dim * 8-byte little-endian f64 ++ LEB128(attr_len)
//!                ++ attr bytes (attr_len 0 = no attribute)
//! ```
//!
//! The elem value does NOT repeat `dim` (the meta record owns it), so
//! every elem decode needs the meta's dim -- callers always resolved the
//! key state first anyway. Element suffixes are RAW bytes, so scanning
//! one key's elems uses the per-kind bounds of [`elems_range`] (the
//! `hash_ds::fields_range` pattern; a family-wide span would swallow
//! other keys' records because the kind byte sorts first). Similarity
//! search is a brute-force scan over those records -- no HNSW index is
//! kept (documented deviation, see COMPAT.md).

use rocksdb::WriteBatch;

use crate::ds::codec::{self, KIND_VECTORSET_ELEM, KIND_VECTORSET_META, VECTORSET_FAMILY};
use crate::ds::expire;
use crate::store::{key_upper_bound, ops, Store};

/// One element's decoded payload: the `dim`-length vector plus the
/// optional attribute blob (`None` = the element has no attr).
pub type ElemValue = (Vec<f64>, Option<Vec<u8>>);

/// Meta/root physical key of a vector set.
pub fn meta_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_VECTORSET_META, key)
}

/// Physical key of one element (suffix = the raw element bytes).
pub fn elem_key(prefix: &[u8], key: &[u8], element: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_VECTORSET_ELEM, key, element)
}

/// Exclusive bounds `[lower, upper)` covering EVERY elem record of `key`
/// and nothing else (per-kind, key-confined via `key_upper_bound`).
pub fn elems_range(prefix: &[u8], key: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lower = codec::data_key(prefix, KIND_VECTORSET_ELEM, key);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
}

/// Root counters of one vector set: TTL, dimension, element count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorSetMeta {
    pub expire_ms: u64,
    pub dim: u64,
    pub count: u64,
}

/// Result of reading a vector set's meta record (mirrors
/// `zset_ds::ZSetMetaRead`).
#[derive(Debug, PartialEq, Eq)]
pub enum VectorSetMetaRead {
    /// No meta record: the vector set does not exist.
    Missing,
    /// Live vector set.
    Present(VectorSetMeta),
    /// Expired: the whole family was just purged.
    Purged,
    /// Store error; callers reply a generic error.
    Failed(String),
}

/// Leading LEB128 varint + the remainder; `None` when unterminated.
/// Overlong inputs (10+ bytes) are impossible from our writers; the
/// first 9 bytes saturate like `codec::decode_envelope`.
fn decode_leb128(payload: &[u8]) -> Option<(u64, &[u8])> {
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

/// Decode the meta payload after the expire envelope: `dim ++ count`.
/// Malformed payloads read as zeros (corruption must not break reads,
/// the `codec::decode_count` contract).
pub fn decode_meta(payload: &[u8]) -> (u64, u64) {
    let (dim, rest) = decode_leb128(payload).unwrap_or((0, &[]));
    let (count, _) = decode_leb128(rest).unwrap_or((0, &[]));
    (dim, count)
}

/// Read + lazily expire one vector set's meta record. Does NOT detect
/// wrong-type keys (a foreign kind simply reads as Missing here); the
/// command layer disambiguates via `keys_core::resolve` first.
pub fn read_meta(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> VectorSetMetaRead {
    let root = meta_key(prefix, key);
    let val = match ops::get_physical(store, &root) {
        Err(e) => return VectorSetMetaRead::Failed(e),
        Ok(None) => return VectorSetMetaRead::Missing,
        Ok(Some(v)) => v,
    };
    let (expire_ms, payload) = codec::decode_envelope(&val);
    if expire::is_expired(expire_ms, now) {
        return if expire::purge_if_expired(store, prefix, VECTORSET_FAMILY, key, now) {
            VectorSetMetaRead::Purged
        } else {
            VectorSetMetaRead::Failed("purge failed".to_string())
        };
    }
    let (dim, count) = decode_meta(payload);
    VectorSetMetaRead::Present(VectorSetMeta {
        expire_ms,
        dim,
        count,
    })
}

/// Put the meta record into `batch`, keeping the TTL envelope and
/// maintaining the expire index entry (old -> new, the `json_ds`
/// `write_doc` pattern; unchanged expiry only re-asserts the entry).
pub fn write_meta(
    batch: &mut WriteBatch,
    prefix: &[u8],
    key: &[u8],
    old_expire: u64,
    expire_ms: u64,
    dim: u64,
    count: u64,
) {
    let root = meta_key(prefix, key);
    let mut payload = codec::encode_count(dim);
    payload.extend_from_slice(&codec::encode_count(count));
    batch.put(&root, codec::encode_envelope(expire_ms, &payload));
    expire::set_ttl_entries(batch, prefix, root, old_expire, expire_ms);
}

/// Batch entries wiping the whole vector-set family and its TTL index.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64) {
    expire::family_delete_entries(batch, prefix, VECTORSET_FAMILY, key, expire_ms);
}

/// Encode one elem value: `dim * LE f64 ++ LEB128(attr_len) ++ attr`.
/// An empty attribute encodes attr_len 0 (indistinguishable from None).
fn encode_elem_value(dim: u64, vector: &[f64], attr: Option<&[u8]>) -> Vec<u8> {
    let attr = attr.filter(|a| !a.is_empty());
    let mut out = Vec::with_capacity(dim as usize * 8 + 1 + attr.map_or(0, |a| a.len()));
    for &x in vector.iter().take(dim as usize) {
        out.extend_from_slice(&x.to_le_bytes());
    }
    match attr {
        Some(a) => {
            out.extend_from_slice(&codec::encode_count(a.len() as u64));
            out.extend_from_slice(a);
        }
        None => out.push(0),
    }
    out
}

/// Decode one elem value against the meta's `dim`; `None` unless the
/// whole value decodes cleanly (8*dim vector bytes, a well-formed
/// attr_len, and EXACTLY attr_len trailing bytes).
pub fn decode_elem_value(value: &[u8], dim: u64) -> Option<ElemValue> {
    let vec_len = dim.checked_mul(8)? as usize;
    let raw = value.get(..vec_len)?;
    let mut vector = Vec::with_capacity(dim as usize);
    for chunk in raw.chunks_exact(8) {
        vector.push(f64::from_le_bytes(chunk.try_into().ok()?));
    }
    let (attr_len, rest) = decode_leb128(value.get(vec_len..)?)?;
    if rest.len() != attr_len as usize {
        return None;
    }
    let attr = (attr_len != 0).then(|| rest.to_vec());
    Some((vector, attr))
}

/// Put one element record into `batch` (vector ++ attr).
pub fn put_elem(
    batch: &mut WriteBatch,
    prefix: &[u8],
    key: &[u8],
    element: &[u8],
    dim: u64,
    vector: &[f64],
    attr: Option<&[u8]>,
) {
    batch.put(
        elem_key(prefix, key, element),
        encode_elem_value(dim, vector, attr),
    );
}

/// One element's `(vector, attr)`; `Ok(None)` = element absent (or the
/// record is corrupt). `dim` comes from the meta record -- the elem
/// value does not embed it.
pub fn read_elem(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    element: &[u8],
    dim: u64,
) -> Result<Option<ElemValue>, String> {
    let val = ops::get_physical(store, &elem_key(prefix, key, element))?;
    Ok(val.as_deref().and_then(|v| decode_elem_value(v, dim)))
}

/// Iterate `key`'s elem records in physical (bytewise element) order:
/// `f(element, raw_value)` returns `false` to stop early; iteration
/// also ends once keys leave `key`'s `KIND_VECTORSET_ELEM` window.
pub fn for_each_elem(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    f: &mut dyn FnMut(&[u8], &[u8]) -> bool,
) -> Result<(), String> {
    let (lower, upper) = elems_range(prefix, key);
    let base = lower.len();
    ops::for_each_from(store, &lower, false, &mut |k, v| {
        if k >= upper.as_slice() {
            return false; // left this key's elem window
        }
        match k.get(base..) {
            Some(element) => f(element, v),
            None => true,
        }
    })
}

/// IEEE 754 binary16 (half) -> f64, hand-rolled (no half crate): sign
/// bit 15, exponent bits 10..=14 (bias 15), fraction bits 0..=9. Exact
/// for every input -- an 11-bit significand scaled by a power of two.
pub fn fp16_to_f64(h: u16) -> f64 {
    let exp = ((h >> 10) & 0x1F) as i32;
    let frac = u64::from(h & 0x03FF);
    let mag = match exp {
        // Subnormal: frac * 2^-24 (implicit 0.fraction, min exponent -14).
        0 => (frac as f64) * (-24f64).exp2(),
        // Largest exponent: +-inf (frac 0) or NaN.
        0x1F if frac == 0 => f64::INFINITY,
        0x1F => f64::NAN,
        // Normal: (1.frac) * 2^(exp-15); the 11-bit significand is exact.
        _ => (((1u64 << 10) | frac) as f64 / 1024.0) * f64::from(exp - 15).exp2(),
    };
    if h & 0x8000 != 0 {
        -mag
    } else {
        mag
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::rocksdb;

    const P: &[u8] = b"70/";

    fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = rocksdb::open(dir.path().to_str().unwrap()).expect("open");
        (dir, store)
    }

    fn put_meta(store: &Store, key: &[u8], old: u64, new: u64, dim: u64, count: u64) {
        let mut batch = WriteBatch::default();
        write_meta(&mut batch, P, key, old, new, dim, count);
        ops::batch_write(store, batch).expect("batch");
    }

    #[test]
    fn meta_roundtrip_and_lazy_purge() {
        let (_dir, store) = open_tmp();
        assert_eq!(read_meta(&store, P, b"v", 0), VectorSetMetaRead::Missing);
        put_meta(&store, b"v", 0, 0, 4, 7);
        assert_eq!(
            read_meta(&store, P, b"v", 0),
            VectorSetMetaRead::Present(VectorSetMeta {
                expire_ms: 0,
                dim: 4,
                count: 7
            })
        );
        // Expired meta purges the family; a re-read is plain Missing.
        put_meta(&store, b"v", 0, 5, 4, 7);
        assert_eq!(read_meta(&store, P, b"v", 10), VectorSetMetaRead::Purged);
        assert_eq!(read_meta(&store, P, b"v", 10), VectorSetMetaRead::Missing);
    }

    #[test]
    fn write_meta_moves_the_ttl_index_entry() {
        let (_dir, store) = open_tmp();
        put_meta(&store, b"v", 0, 100, 2, 1);
        let old_idx = codec::expire_index_key(P, 100, &meta_key(P, b"v"));
        assert!(ops::get_physical(&store, &old_idx).unwrap().is_some());
        // old -> new: the stale deadline entry is deleted, not duplicated.
        put_meta(&store, b"v", 100, 200, 2, 2);
        assert!(ops::get_physical(&store, &old_idx).unwrap().is_none());
        let new_idx = codec::expire_index_key(P, 200, &meta_key(P, b"v"));
        assert!(ops::get_physical(&store, &new_idx).unwrap().is_some());
    }

    #[test]
    fn elem_roundtrip_with_and_without_attr() {
        for dim in 1u64..=3 {
            let vector: Vec<f64> = (0..dim).map(|i| (i as f64) * 0.5 - 0.25).collect();
            let attrs: [Option<&[u8]>; 4] =
                [None, Some(b""), Some(b"year=2026"), Some(b"\x00\xff")];
            for attr in attrs {
                let enc = encode_elem_value(dim, &vector, attr);
                let dec = decode_elem_value(&enc, dim);
                let want = attr.filter(|a| !a.is_empty()).map(<[u8]>::to_vec);
                assert_eq!(dec, Some((vector.clone(), want)), "dim {dim}");
            }
        }
        // Wrong dim cannot decode cleanly (trailing/missing bytes).
        let enc = encode_elem_value(2, &[1.0, 2.0], Some(b"a"));
        assert_eq!(decode_elem_value(&enc, 3), None);
        assert_eq!(decode_elem_value(&enc, 1), None);
        // Truncated attr: attr_len promises bytes that are not there.
        let mut short = encode_elem_value(1, &[1.0], Some(b"abcd"));
        short.pop();
        assert_eq!(decode_elem_value(&short, 1), None);
    }

    #[test]
    fn for_each_elem_is_confined_to_one_key() {
        let (_dir, store) = open_tmp();
        put_meta(&store, b"k", 0, 0, 1, 2);
        put_meta(&store, b"k2", 0, 0, 1, 1);
        let mut batch = WriteBatch::default();
        put_elem(&mut batch, P, b"k", b"b", 1, &[2.0], Some(b"attr"));
        put_elem(&mut batch, P, b"k", b"a", 1, &[1.0], None);
        put_elem(&mut batch, P, b"k2", b"a", 1, &[9.0], None);
        ops::batch_write(&store, batch).expect("batch");
        let mut seen: Vec<(Vec<u8>, Option<Vec<f64>>)> = Vec::new();
        for_each_elem(&store, P, b"k", &mut |elem, value| {
            seen.push((elem.to_vec(), decode_elem_value(value, 1).map(|(v, _)| v)));
            true
        })
        .expect("scan");
        // "k2" stays out even though its root key bytes extend "k"'s.
        assert_eq!(
            seen,
            vec![
                (b"a".to_vec(), Some(vec![1.0])),
                (b"b".to_vec(), Some(vec![2.0])),
            ]
        );
        // Early stop propagates.
        let mut first = 0;
        for_each_elem(&store, P, b"k", &mut |_, _| {
            first += 1;
            false
        })
        .expect("scan");
        assert_eq!(first, 1);
    }

    #[test]
    fn delete_family_removes_meta_elems_and_index() {
        let (_dir, store) = open_tmp();
        put_meta(&store, b"v", 0, 77, 2, 1);
        let mut batch = WriteBatch::default();
        put_elem(&mut batch, P, b"v", b"e", 2, &[1.0, 2.0], None);
        ops::batch_write(&store, batch).expect("batch");
        let idx = codec::expire_index_key(P, 77, &meta_key(P, b"v"));
        assert!(ops::get_physical(&store, &idx).unwrap().is_some());
        let mut wipe = WriteBatch::default();
        delete_family(&mut wipe, P, b"v", 77);
        ops::batch_write(&store, wipe).expect("batch");
        assert_eq!(read_meta(&store, P, b"v", 0), VectorSetMetaRead::Missing);
        assert_eq!(read_elem(&store, P, b"v", b"e", 2), Ok(None));
        assert!(ops::get_physical(&store, &idx).unwrap().is_none());
    }

    #[test]
    fn fp16_decodes_the_reference_points() {
        assert_eq!(fp16_to_f64(0x0000), 0.0);
        assert_eq!(fp16_to_f64(0x8000).to_bits(), (-0.0f64).to_bits());
        assert_eq!(fp16_to_f64(0x3C00), 1.0);
        assert_eq!(fp16_to_f64(0x4000), 2.0);
        assert_eq!(fp16_to_f64(0xC000), -2.0);
        assert_eq!(fp16_to_f64(0x3800), 0.5);
        assert_eq!(fp16_to_f64(0x0001), (-24f64).exp2());
        assert_eq!(fp16_to_f64(0x7C00), f64::INFINITY);
        assert_eq!(fp16_to_f64(0xFC00), f64::NEG_INFINITY);
        assert!(fp16_to_f64(0x7E00).is_nan());
        assert!((fp16_to_f64(0x3555) - 0.333251953125f64).abs() < 1e-12);
        // Largest finite half: (2 - 2^-10) * 2^15 = 65504.
        assert_eq!(fp16_to_f64(0x7BFF), 65504.0);
        // Smallest normal half: 2^-14.
        assert_eq!(fp16_to_f64(0x0400), (-14f64).exp2());
    }
}
