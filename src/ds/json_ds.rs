//! JSON document storage ops (name `json_ds` mirrors `hash_ds`/`set_ds`).
//! Physical layout, ONE record per user key -- unlike the field/member
//! families there are no per-element records:
//!
//! ```text
//! doc = data_key(prefix, KIND_JSON, key)
//!       value = envelope ++ compact serde_json serialization of the
//!               whole document (serde_json `preserve_order` keeps object
//!               key insertion order, so JSON.SET -> JSON.GET is
//!               byte-stable without a re-formatting pass)
//! ```
//!
//! Every mutation therefore reads the whole document, edits the
//! serde_json tree in memory and rewrites the single record in ONE
//! batched fsync under the per-key latch (command layer). TTL is the
//! uniform envelope + index-entry pair: [`write_doc`] keeps the index in
//! step (old_expire -> new_expire, the `write_meta` maintenance pattern)
//! and [`delete_family`] wipes record + index in one family range.

use rocksdb::WriteBatch;

use crate::ds::codec::{self, JSON_FAMILY, KIND_JSON};
use crate::ds::expire;
use crate::store::{ops, Store};

/// Root physical key of one JSON document.
pub fn doc_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_JSON, key)
}

/// Result of reading one JSON document record.
#[derive(Debug, PartialEq, Eq)]
pub enum JsonRead {
    /// No record: the key does not exist (as a JSON document).
    Missing,
    /// Live document: absolute expiry (0 = none) and the raw compact
    /// serialization (parse it at the command layer).
    Present { expire_ms: u64, doc: Vec<u8> },
    /// Expired: the record + index entry were just purged.
    Purged,
    /// Store error; callers reply a generic error.
    Failed(String),
}

/// Read + lazily expire one JSON document. Does NOT detect wrong-type
/// keys (a foreign kind simply reads as Missing here); the command layer
/// disambiguates via `keys_core::resolve` first.
pub fn read(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> JsonRead {
    let root = doc_key(prefix, key);
    let val = match ops::get_physical(store, &root) {
        Err(e) => return JsonRead::Failed(e),
        Ok(None) => return JsonRead::Missing,
        Ok(Some(v)) => v,
    };
    let (expire_ms, payload) = codec::decode_envelope(&val);
    if expire::is_expired(expire_ms, now) {
        return if expire::purge_if_expired(store, prefix, JSON_FAMILY, key, now) {
            JsonRead::Purged
        } else {
            JsonRead::Failed("purge failed".to_string())
        };
    }
    JsonRead::Present {
        expire_ms,
        doc: payload.to_vec(),
    }
}

/// Put the document record into `batch`, keeping the TTL envelope and
/// maintaining the expire index entry (old -> new; unchanged expiry only
/// re-asserts the entry, exactly like the meta writers of the other
/// families).
pub fn write_doc(
    batch: &mut WriteBatch,
    prefix: &[u8],
    key: &[u8],
    old_expire: u64,
    new_expire: u64,
    doc: &[u8],
) {
    let root = doc_key(prefix, key);
    batch.put(&root, codec::encode_envelope(new_expire, doc));
    expire::set_ttl_entries(batch, prefix, root, old_expire, new_expire);
}

/// Batch entries wiping the JSON record (one range over the single-kind
/// family) plus its TTL index entry.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64) {
    expire::family_delete_entries(batch, prefix, JSON_FAMILY, key, expire_ms);
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

    fn put_doc(store: &Store, key: &[u8], old: u64, new: u64, doc: &[u8]) {
        let mut batch = WriteBatch::default();
        write_doc(&mut batch, P, key, old, new, doc);
        ops::batch_write(store, batch).expect("batch");
    }

    #[test]
    fn doc_roundtrip_and_lazy_purge() {
        let (_dir, store) = open_tmp();
        assert_eq!(read(&store, P, b"j", 0), JsonRead::Missing);
        put_doc(&store, b"j", 0, 0, br#"{"a":[1,2]}"#);
        assert_eq!(
            read(&store, P, b"j", 0),
            JsonRead::Present {
                expire_ms: 0,
                doc: br#"{"a":[1,2]}"#.to_vec()
            }
        );
        // Expired record purges itself; a re-read is plain Missing.
        put_doc(&store, b"j", 0, 5, b"[1]");
        assert_eq!(read(&store, P, b"j", 10), JsonRead::Purged);
        assert_eq!(read(&store, P, b"j", 10), JsonRead::Missing);
    }

    #[test]
    fn write_doc_moves_the_ttl_index_entry() {
        let (_dir, store) = open_tmp();
        put_doc(&store, b"j", 0, 100, b"1");
        let old_idx = codec::expire_index_key(P, 100, &doc_key(P, b"j"));
        assert!(ops::get_physical(&store, &old_idx).unwrap().is_some());
        // old -> new: the stale deadline entry is deleted, not duplicated.
        put_doc(&store, b"j", 100, 200, b"2");
        assert!(ops::get_physical(&store, &old_idx).unwrap().is_none());
        let new_idx = codec::expire_index_key(P, 200, &doc_key(P, b"j"));
        assert!(ops::get_physical(&store, &new_idx).unwrap().is_some());
    }

    #[test]
    fn delete_family_removes_record_and_index() {
        let (_dir, store) = open_tmp();
        put_doc(&store, b"j", 0, 77, b"null");
        let idx = codec::expire_index_key(P, 77, &doc_key(P, b"j"));
        assert!(ops::get_physical(&store, &idx).unwrap().is_some());
        let mut batch = WriteBatch::default();
        delete_family(&mut batch, P, b"j", 77);
        ops::batch_write(&store, batch).expect("batch");
        assert_eq!(read(&store, P, b"j", 0), JsonRead::Missing);
        assert!(ops::get_physical(&store, &idx).unwrap().is_none());
    }
}
