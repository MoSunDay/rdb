//! Set storage ops. Physical layout, one user key per set:
//!
//! ```text
//! meta   = data_key(prefix, KIND_SET_META, key)     value = envelope ++ LEB128(count)
//! member = elem_key(prefix, KIND_SET_MEMBER, key, member)  value = b"" (existence
//!                                                          encoding; the member IS
//!                                                          the key)
//! ```
//!
//! Member suffixes are RAW bytes after `<kind><len><key>`; scanning one
//! set uses the per-kind bounds of [`members_range`] so other keys'
//! members never leak in (the kind byte sorts before the key bytes).

use rocksdb::WriteBatch;

use crate::ds::codec::{self, KIND_SET_MEMBER, KIND_SET_META, SET_FAMILY};
use crate::ds::expire;
use crate::store::{key_upper_bound, ops, Store};

/// Meta/root physical key of a set.
pub fn meta_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_SET_META, key)
}

/// Physical key of one member (suffix = the raw member bytes).
pub fn member_key(prefix: &[u8], key: &[u8], member: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_SET_MEMBER, key, member)
}

/// Exclusive bounds `[lower, upper)` covering EVERY member record of `key`.
pub fn members_range(prefix: &[u8], key: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let lower = codec::data_key(prefix, KIND_SET_MEMBER, key);
    let upper = key_upper_bound(&lower).unwrap_or_default();
    (lower, upper)
}

/// Result of reading a set's meta record.
#[derive(Debug, PartialEq, Eq)]
pub enum MetaRead {
    /// No meta record: the set does not exist.
    Missing,
    /// Live set: absolute expiry (0 = none) and member count.
    Present { expire_ms: u64, count: u64 },
    /// Expired: the whole family was just purged.
    Purged,
    /// Store error.
    Failed(String),
}

/// Read + lazily expire one set's meta. Wrong-type detection is the
/// command layer's job (via `keys_core::resolve`).
pub fn read_meta(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> MetaRead {
    let root = meta_key(prefix, key);
    let val = match ops::get_physical(store, &root) {
        Err(e) => return MetaRead::Failed(e),
        Ok(None) => return MetaRead::Missing,
        Ok(Some(v)) => v,
    };
    let (expire_ms, payload) = codec::decode_envelope(&val);
    if expire::is_expired(expire_ms, now) {
        return if expire::purge_if_expired(store, prefix, SET_FAMILY, key, now) {
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

/// Put the meta record into `batch`, keeping the TTL envelope.
pub fn write_meta(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64, count: u64) {
    batch.put(
        meta_key(prefix, key),
        codec::encode_envelope(expire_ms, &codec::encode_count(count)),
    );
}

/// Batch entries wiping the whole set family and its TTL index entry.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64) {
    expire::family_delete_entries(batch, prefix, SET_FAMILY, key, expire_ms);
}

/// Membership check; store errors read as "absent" (best-effort).
pub fn has_member(store: &Store, prefix: &[u8], key: &[u8], member: &[u8]) -> Result<bool, String> {
    Ok(ops::get_physical(store, &member_key(prefix, key, member))?.is_some())
}

/// One page of members in physical (bytewise member) order. `next` = the
/// last MATCHED member's bytes; empty when iteration finished.
pub struct MemberPage {
    pub members: Vec<Vec<u8>>,
    pub next: Vec<u8>,
}

/// Collect up to `count` members (0 = unbounded), optionally glob-filtered,
/// starting strictly after `from_member` (None = first member).
pub fn collect_members(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    from_member: Option<&[u8]>,
    pattern: Option<&[u8]>,
    count: usize,
) -> MemberPage {
    let (lower, upper) = members_range(prefix, key);
    let (start, excl_start) = match from_member {
        Some(m) => (codec::elem_key(prefix, KIND_SET_MEMBER, key, m), true),
        None => (lower.clone(), false),
    };
    let mut members: Vec<Vec<u8>> = Vec::new();
    let mut resume: Option<Vec<u8>> = None;
    let base = lower.len();
    let _ = ops::for_each_from(store, &start, excl_start, &mut |k, _| {
        if k >= upper.as_slice() {
            return false; // left this set's member window
        }
        if let Some(member) = k.get(base..) {
            if pattern.is_none_or(|p| crate::utils::glob_match(p, member)) {
                members.push(member.to_vec());
                if count != 0 && members.len() >= count {
                    resume = Some(member.to_vec());
                    return false;
                }
            }
        }
        true
    });
    MemberPage {
        members,
        next: resume.unwrap_or_default(),
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

    fn write_set(store: &Store, key: &[u8], expire_ms: u64, members: &[&[u8]]) {
        let mut batch = WriteBatch::default();
        write_meta(&mut batch, P, key, expire_ms, members.len() as u64);
        if expire_ms > 0 {
            batch.put(
                codec::expire_index_key(P, expire_ms, &meta_key(P, key)),
                b"",
            );
        }
        for m in members {
            batch.put(member_key(P, key, m), b"");
        }
        ops::batch_write(store, batch).expect("batch");
    }

    #[test]
    fn meta_roundtrip_and_lazy_purge() {
        let (_dir, store) = open_tmp();
        assert_eq!(read_meta(&store, P, b"s", 0), MetaRead::Missing);
        write_set(&store, b"s", 0, &[b"a", b"b"]);
        assert_eq!(
            read_meta(&store, P, b"s", 0),
            MetaRead::Present {
                expire_ms: 0,
                count: 2
            }
        );
        assert!(has_member(&store, P, b"s", b"a").unwrap());
        assert!(!has_member(&store, P, b"s", b"z").unwrap());
        write_set(&store, b"s", 5, &[b"a"]);
        assert_eq!(read_meta(&store, P, b"s", 10), MetaRead::Purged);
        assert!(!has_member(&store, P, b"s", b"a").unwrap());
    }

    #[test]
    fn member_window_is_key_confined() {
        let (_dir, store) = open_tmp();
        write_set(&store, b"s1", 0, &[b"a", b"z"]);
        write_set(&store, b"s1x", 0, &[b"leak"]);
        let page = collect_members(&store, P, b"s1", None, None, 0);
        assert_eq!(page.members, vec![b"a".to_vec(), b"z".to_vec()]);
        assert!(page.next.is_empty());
    }

    #[test]
    fn collect_pages_filter_and_resume() {
        let (_dir, store) = open_tmp();
        write_set(&store, b"s", 0, &[b"m1", b"m2", b"n3"]);
        let p1 = collect_members(&store, P, b"s", None, Some(b"m?"), 1);
        assert_eq!(p1.members, vec![b"m1".to_vec()]);
        assert_eq!(p1.next, b"m1".to_vec());
        let p2 = collect_members(&store, P, b"s", Some(&p1.next), Some(b"m?"), 1);
        assert_eq!(p2.members, vec![b"m2".to_vec()]);
        let p3 = collect_members(&store, P, b"s", Some(b"n3"), None, 5);
        assert!(p3.members.is_empty() && p3.next.is_empty());
        assert!(collect_members(&store, P, b"gone", None, None, 0)
            .members
            .is_empty());
    }

    #[test]
    fn delete_family_removes_members_and_index() {
        let (_dir, store) = open_tmp();
        write_set(&store, b"s", 88, &[b"a"]);
        let idx = codec::expire_index_key(P, 88, &meta_key(P, b"s"));
        assert!(ops::get_physical(&store, &idx).unwrap().is_some());
        let mut batch = WriteBatch::default();
        delete_family(&mut batch, P, b"s", 88);
        ops::batch_write(&store, batch).expect("batch");
        assert_eq!(read_meta(&store, P, b"s", 0), MetaRead::Missing);
        assert!(!has_member(&store, P, b"s", b"a").unwrap());
        assert!(ops::get_physical(&store, &idx).unwrap().is_none());
    }
}
