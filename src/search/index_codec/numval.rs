//! Numeric doc-values record codec (kind 0x19, NUMERIC fields): one
//! bare f64 per (index, field, docid) -- no LEB128 envelope, the value
//! is just 8 little-endian bytes so a field scan can decode in place.
//! Kept in its own kind (not folded into the doc record) because it is
//! per-FIELD columnar state that must survive doc-record format
//! changes; the same family span still wipes it on DROP/purge.

use crate::ds::codec::{elem_key, encode_count, KIND_SEARCH_NUMVAL};
use crate::store::key_upper_bound;

/// Physical key of one doc's numeric value.
pub fn numval_key(prefix: &[u8], index: &[u8], field: &[u8], docid: &[u8]) -> Vec<u8> {
    let mut suffix = encode_count(field.len() as u64);
    suffix.extend_from_slice(field);
    suffix.extend_from_slice(docid);
    elem_key(prefix, KIND_SEARCH_NUMVAL, index, &suffix)
}

/// 8 bytes LE.
pub fn encode_numval(v: f64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

/// `None` on truncated payloads.
pub fn decode_numval(raw: &[u8]) -> Option<f64> {
    Some(f64::from_le_bytes(raw.get(..8)?.try_into().ok()?))
}

/// `[lower, upper)` span of one field's numval records (field prefix
/// encoded once; the +1-length trick cannot cross into another field
/// because the LEB128 length grows before the bytes -- the
/// `ann_posting_range` pattern).
pub fn numval_range(prefix: &[u8], index: &[u8], field: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut suffix = encode_count(field.len() as u64);
    suffix.extend_from_slice(field);
    let lower = elem_key(prefix, KIND_SEARCH_NUMVAL, index, &suffix);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numval_roundtrip_and_range() {
        let (lower, upper) = numval_range(b"42/", b"idx", b"price");
        assert!(lower < upper);
        // +1-length trick: upper is lower with its last byte bumped
        let common = lower.len() - 1;
        assert_eq!(&upper[..common], &lower[..common]);
        // the key of any docid of this field sorts inside the span
        assert!(numval_key(b"42/", b"idx", b"price", b"d1") > lower);
        assert!(numval_key(b"42/", b"idx", b"price", b"d1") < upper);
        // another same-length field ("prize" > "pricf") stays past the
        // span; a shorter name ("qty") sorts before it via the length
        // byte, so the span covers exactly this field's records
        assert!(numval_key(b"42/", b"idx", b"prize", b"d1") > upper);
        for &v in &[0.0f64, -1.5, 3.5e300] {
            let raw = encode_numval(v);
            assert_eq!(raw.len(), 8);
            assert_eq!(decode_numval(&raw), Some(v));
        }
        assert_eq!(decode_numval(&[0u8; 7]), None);
    }
}
