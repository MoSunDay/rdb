//! Hash storage ops (name `hash_ds` avoids clashing with the `crate::hash`
//! slot module). Physical layout, one user key per hash:
//!
//! ```text
//! meta  = data_key(prefix, KIND_HASH_META, key)   value = envelope ++ LEB128(count)
//! field = elem_key(prefix, KIND_HASH_FLD, key, field)   value = raw bytes (no
//!                                                       envelope; TTL is per-key
//!                                                       through the meta record)
//! ```
//!
//! Field suffixes are RAW bytes after `<kind><len><key>`, so scanning one
//! hash's fields uses the per-kind bounds of [`fields_range`]; a family-wide
//! span would swallow other keys' fields (kind byte sorts first).

use rocksdb::WriteBatch;

use crate::ds::codec::{self, HASH_FAMILY, KIND_HASH_FLD, KIND_HASH_META};
use crate::ds::expire;
use crate::store::{key_upper_bound, ops, Store};

/// Meta/root physical key of a hash.
pub fn meta_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_HASH_META, key)
}

/// Physical key of one field (suffix = the raw field bytes).
pub fn field_key(prefix: &[u8], key: &[u8], field: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_HASH_FLD, key, field)
}

/// Exclusive bounds `[lower, upper)` covering EVERY field record of `key`
/// and nothing else (per-kind, key-confined via `key_upper_bound`).
pub fn fields_range(prefix: &[u8], key: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lower = codec::data_key(prefix, KIND_HASH_FLD, key);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
}

/// Result of reading a hash's meta record.
#[derive(Debug, PartialEq, Eq)]
pub enum MetaRead {
    /// No meta record: the hash does not exist.
    Missing,
    /// Live hash: absolute expiry (0 = none) and field count.
    Present { expire_ms: u64, count: u64 },
    /// Expired: the whole family (meta + fields + index) was just purged.
    Purged,
    /// Store error; callers reply a generic error.
    Failed(String),
}

/// Read + lazily expire one hash's meta. Does NOT detect wrong-type keys
/// (a non-hash meta record simply reads as Missing here); the command
/// layer disambiguates via `keys_core::resolve` first.
pub fn read_meta(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> MetaRead {
    let root = meta_key(prefix, key);
    let val = match ops::get_physical(store, &root) {
        Err(e) => return MetaRead::Failed(e),
        Ok(None) => return MetaRead::Missing,
        Ok(Some(v)) => v,
    };
    let (expire_ms, payload) = codec::decode_envelope(&val);
    if expire::is_expired(expire_ms, now) {
        return if expire::purge_if_expired(store, prefix, HASH_FAMILY, key, now) {
            MetaRead::Purged
        } else {
            MetaRead::Failed("purge failed".to_string())
        };
    }
    MetaRead::Present {
        expire_ms,
        count: codec::decode_count(payload),
    }
}

/// Put the meta record (envelope keeps the existing TTL) into `batch`.
pub fn write_meta(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64, count: u64) {
    batch.put(
        meta_key(prefix, key),
        codec::encode_envelope(expire_ms, &codec::encode_count(count)),
    );
}

/// Batch entries wiping the whole hash family and its TTL index entry.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64) {
    expire::family_delete_entries(batch, prefix, HASH_FAMILY, key, expire_ms);
}

/// One field's value; `Ok(None)` = field absent.
pub fn read_field(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    field: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    ops::get_physical(store, &field_key(prefix, key, field))
}

/// One page of `[field, value]` pairs in physical (bytewise field) order.
/// `next` = Some(last MATCHED field) is the resume cursor; None means
/// iteration finished (an EMPTY field is a valid field, so the cursor is
/// an Option, not an empty-bytes sentinel). `from_field` resumes strictly
/// after that field.
pub struct FieldPage {
    pub fields: Vec<(Vec<u8>, Vec<u8>)>,
    pub next: Option<Vec<u8>>,
}

/// Collect up to `count` fields (0 = unbounded), optionally glob-filtered,
/// starting after `from_field` (None = from the first field).
pub fn collect_fields(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    from_field: Option<&[u8]>,
    pattern: Option<&[u8]>,
    count: usize,
) -> FieldPage {
    let (lower, upper) = fields_range(prefix, key);
    let (start, excl_start) = match from_field {
        Some(f) => (codec::elem_key(prefix, KIND_HASH_FLD, key, f), true),
        None => (lower.clone(), false),
    };
    let mut fields: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut resume: Option<Vec<u8>> = None;
    let base = lower.len();
    let _ = ops::for_each_from(store, &start, excl_start, &mut |k, v| {
        if k >= upper.as_slice() {
            return false; // left this hash's field window
        }
        if let Some(field) = k.get(base..) {
            if pattern.is_none_or(|p| crate::utils::glob_match(p, field)) {
                fields.push((field.to_vec(), v.to_vec()));
                if count != 0 && fields.len() >= count {
                    resume = Some(field.to_vec());
                    return false;
                }
            }
        }
        true
    });
    FieldPage {
        fields,
        next: resume,
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

    fn write_hash(store: &Store, key: &[u8], expire_ms: u64, pairs: &[(&[u8], &[u8])]) {
        let mut batch = WriteBatch::default();
        write_meta(&mut batch, P, key, expire_ms, pairs.len() as u64);
        if expire_ms > 0 {
            batch.put(
                codec::expire_index_key(P, expire_ms, &meta_key(P, key)),
                b"",
            );
        }
        for (f, v) in pairs {
            batch.put(field_key(P, key, f), *v);
        }
        ops::batch_write(store, batch).expect("batch");
    }

    #[test]
    fn meta_roundtrip_and_lazy_purge() {
        let (_dir, store) = open_tmp();
        assert_eq!(read_meta(&store, P, b"h", 0), MetaRead::Missing);
        write_hash(&store, b"h", 0, &[(b"a", b"1")]);
        assert_eq!(
            read_meta(&store, P, b"h", 0),
            MetaRead::Present {
                expire_ms: 0,
                count: 1
            }
        );
        // Expired meta purges the whole family; fields are gone too.
        write_hash(&store, b"h", 5, &[(b"a", b"1")]);
        assert_eq!(read_meta(&store, P, b"h", 10), MetaRead::Purged);
        assert_eq!(read_meta(&store, P, b"h", 10), MetaRead::Missing);
        assert_eq!(read_field(&store, P, b"h", b"a"), Ok(None));
    }

    #[test]
    fn field_window_is_key_confined() {
        let (_dir, store) = open_tmp();
        write_hash(&store, b"h1", 0, &[(b"a", b"1"), (b"z", b"2")]);
        write_hash(&store, b"h2", 0, &[(b"b", b"3")]);
        // h2's field must not appear inside h1's window (or vice versa).
        let page = collect_fields(&store, P, b"h1", None, None, 0);
        let got: Vec<Vec<u8>> = page.fields.iter().map(|(f, _)| f.clone()).collect();
        assert_eq!(got, vec![b"a".to_vec(), b"z".to_vec()]);
        assert!(page.next.is_none(), "unbounded read finished");
        assert_eq!(read_field(&store, P, b"h2", b"b"), Ok(Some(b"3".to_vec())));
        // Another key whose bytes extend h1's root ("h1x") stays separate.
        write_hash(&store, b"h1x", 0, &[(b"a", b"9")]);
        let page = collect_fields(&store, P, b"h1", None, None, 0);
        assert_eq!(page.fields.len(), 2, "h1x fields excluded");
    }

    #[test]
    fn collect_pages_filter_and_resume() {
        let (_dir, store) = open_tmp();
        let pairs: Vec<(&[u8], &[u8])> = vec![(b"f1", b"1"), (b"f2", b"2"), (b"g3", b"3")];
        write_hash(&store, b"h", 0, &pairs);
        let p1 = collect_fields(&store, P, b"h", None, Some(b"f*"), 1);
        assert_eq!(p1.fields, vec![(b"f1".to_vec(), b"1".to_vec())]);
        assert_eq!(p1.next, Some(b"f1".to_vec()));
        let p2 = collect_fields(&store, P, b"h", p1.next.as_deref(), Some(b"f*"), 1);
        assert_eq!(p2.fields, vec![(b"f2".to_vec(), b"2".to_vec())]);
        assert_eq!(p2.next, Some(b"f2".to_vec()), "page 2 also stopped at its limit");
        // A cursor past every matching field returns an empty page.
        let p3 = collect_fields(&store, P, b"h", p2.next.as_deref(), Some(b"f*"), 1);
        assert!(p3.fields.is_empty() && p3.next.is_none());
        // Missing key: empty page, no error.
        assert!(collect_fields(&store, P, b"nope", None, None, 0)
            .fields
            .is_empty());
    }

    #[test]
    fn empty_field_is_a_valid_resume_cursor() {
        let (_dir, store) = open_tmp();
        write_hash(
            &store,
            b"h",
            0,
            &[(b"", b"0"), (b"a", b"1"), (b"b", b"2")],
        );
        // A page cutting exactly AT the empty field carries Some(b""),
        // not the finished sentinel.
        let p1 = collect_fields(&store, P, b"h", None, None, 1);
        assert_eq!(p1.fields, vec![(Vec::new(), b"0".to_vec())]);
        assert_eq!(p1.next, Some(Vec::new()));
        // Some(b"") resumes strictly after "": the rest still flows.
        let p2 = collect_fields(&store, P, b"h", p1.next.as_deref(), None, 1);
        assert_eq!(p2.fields, vec![(b"a".to_vec(), b"1".to_vec())]);
        let p3 = collect_fields(&store, P, b"h", p2.next.as_deref(), None, 5);
        assert_eq!(p3.fields, vec![(b"b".to_vec(), b"2".to_vec())]);
        assert_eq!(p3.next, None, "true end only after the last field");
        let all = collect_fields(&store, P, b"h", None, None, 0);
        assert_eq!(all.fields.len(), 3);
        assert_eq!(all.next, None);
    }

    #[test]
    fn delete_family_removes_meta_fields_and_index() {
        let (_dir, store) = open_tmp();
        write_hash(&store, b"h", 77, &[(b"a", b"1")]);
        let idx = codec::expire_index_key(P, 77, &meta_key(P, b"h"));
        assert!(ops::get_physical(&store, &idx).unwrap().is_some());
        let mut batch = WriteBatch::default();
        delete_family(&mut batch, P, b"h", 77);
        ops::batch_write(&store, batch).expect("batch");
        assert_eq!(read_meta(&store, P, b"h", 0), MetaRead::Missing);
        assert_eq!(read_field(&store, P, b"h", b"a"), Ok(None));
        assert!(ops::get_physical(&store, &idx).unwrap().is_none());
    }
}
