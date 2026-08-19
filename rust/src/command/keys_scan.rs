//! User-key enumeration for SCAN, KEYS and RANDOMKEY.
//!
//! A physical key names a whole user key when it is a bare legacy string
//! (`<prefix> ++ <key>`) or carries a META kind; element records (hash
//! fields, list nodes, ...) and 0xFD expire-index entries are internal and
//! skipped. SCAN cursors are opaque: the hex of the last physical key
//! visited, `""`/`"0"` = start. Like Redis, SCAN does NOT lazily expire
//! keys mid-iteration (they may surface until a later purge).

use crate::ds::codec;
use crate::ds::expire;
use crate::store::{ops, Store};

/// A whole user key plus its TYPE name: raw legacy records and the
/// STRING_TTL envelope are both "string", typed META records map via
/// `ds::type_name`. Internal records (elements, expire index) yield
/// `None`.
fn user_entry_of(physical: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let plen = expire::slot_prefix_len(physical)?;
    let after = physical.get(plen..)?;
    match codec::classify(after) {
        codec::Classification::Raw => Some((after.to_vec(), "string")),
        codec::Classification::Typed(kind) if kind == codec::KIND_EXPIRE_INDEX => None,
        codec::Classification::Typed(kind) if codec::is_user_key_kind(kind) => {
            codec::decode_data_key(physical, plen)
                .map(|(_, key, _)| (key, crate::ds::type_name(kind)))
        }
        codec::Classification::Typed(_) => None,
    }
}

/// If `physical` is a whole user key, return the user key; internal record
/// kinds and expire-index entries yield `None`. The slot-prefix length is
/// derived from the physical key itself, so this also works for scans that
/// cross slots (RANDOMKEY).
pub fn user_key_of(physical: &[u8]) -> Option<Vec<u8>> {
    user_entry_of(physical).map(|(key, _)| key)
}

/// Case-insensitive ASCII equality for type names (Redis matches
/// `TYPE ReJSON-RL` case-insensitively).
fn type_name_is(name: &[u8], want: &str) -> bool {
    crate::command::zset_util::eq_ignore_case(name, want.as_bytes())
}

/// Is `name` a type name `SCAN TYPE` accepts? The set is exactly what
/// `ds::type_name` can answer for a real key ("none" is not a type).
pub fn is_scan_type_name(name: &[u8]) -> bool {
    [
        "string",
        "list",
        "set",
        "zset",
        "hash",
        "stream",
        "ReJSON-RL",
        "vectorset",
    ]
    .iter()
    .any(|t| type_name_is(name, t))
}

/// One SCAN page inside `prefix`: up to `count` user keys (optionally
/// glob-filtered by `pattern` and/or type-filtered by `type_filter`,
/// compared AFTER the pattern match) starting after `from`. `next` is the
/// resume cursor (the last physical key visited); empty = iteration
/// finished. The cursor is exclusive: callers pass the previous page's
/// `next` back in. Only RETURNED keys count towards `count`, so a TYPE
/// filter may examine many records per page and still return few.
/// Returns Err when the underlying iterator fails -- callers must NOT
/// turn that into a "finished" (empty) cursor, which would silently
/// truncate a client's iteration.
pub struct ScanPage {
    pub keys: Vec<Vec<u8>>,
    pub next: Vec<u8>,
}

pub fn collect_user_keys(
    store: &Store,
    prefix: &[u8],
    from: &[u8],
    pattern: Option<&[u8]>,
    type_filter: Option<&[u8]>,
    count: usize,
) -> Result<ScanPage, String> {
    // Clamp a foreign cursor into this slot (SCAN is per-slot here).
    let (start, excl) = if from < prefix {
        (prefix.to_vec(), false)
    } else {
        (from.to_vec(), true)
    };
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut resume: Option<Vec<u8>> = None;
    ops::for_each_from(store, &start, excl, &mut |k, _| {
        if !k.starts_with(prefix) {
            return false; // left the slot: done
        }
        if let Some((user, type_name)) = user_entry_of(k) {
            let pattern_ok = pattern.is_none_or(|p| crate::utils::glob_match(p, &user));
            let type_ok = type_filter.is_none_or(|t| type_name_is(t, type_name));
            if pattern_ok && type_ok {
                keys.push(user);
                if keys.len() >= count {
                    resume = Some(k.to_vec());
                    return false;
                }
            }
        }
        true
    })?;
    Ok(ScanPage {
        keys,
        next: resume.unwrap_or_default(),
    })
}

/// Every user key in the slot (KEYS; unbounded count, no type filter).
/// Err on storage failure -- KEYS must not reply a partial key list.
pub fn all_user_keys(
    store: &Store,
    prefix: &[u8],
    pattern: Option<&[u8]>,
) -> Result<Vec<Vec<u8>>, String> {
    Ok(collect_user_keys(store, prefix, prefix, pattern, None, usize::MAX)?.keys)
}

/// RANDOMKEY backend: the first user key at/after a uniformly random slot
/// prefix, wrapping once to the database start when the tail is empty.
/// Err on storage failure (a miss must mean "database empty", not "read
/// failed").
pub fn random_user_key(store: &Store) -> Result<Option<Vec<u8>>, String> {
    let slot = crate::utils::rand_u64() % 16_384;
    let from = format!("{slot}/").into_bytes();
    let physical = match first_user_key_from(store, &from)? {
        Some(k) => Some(k),
        None => first_user_key_from(store, b"")?,
    };
    Ok(physical.and_then(|k| user_key_of(&k)))
}

fn first_user_key_from(store: &Store, from: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut found: Option<Vec<u8>> = None;
    ops::for_each_from(store, from, false, &mut |k, _| {
        if user_key_of(k).is_some() {
            found = Some(k.to_vec());
            false
        } else {
            true
        }
    })?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testutil;

    const P: &[u8] = b"07/";

    /// Guard FIRST: `shared_with` wipes the shared `/tmp/rdb-test-{pid}`
    /// tree, so every store-opening test holds the crate-wide lock.
    fn shared() -> (std::sync::MutexGuard<'static, ()>, crate::state::Shared) {
        let guard = crate::command::string::TEST_STORE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (guard, testutil::shared_with(testutil::test_config()))
    }

    fn put_raw(shared: &crate::state::Shared, key: &[u8], val: &[u8]) {
        crate::store::set(&shared.store, P, key, val).expect("put raw");
    }

    #[test]
    fn user_key_of_splits_raw_meta_elements_and_index() {
        assert_eq!(user_key_of(b"07/plain"), Some(b"plain".to_vec()));
        let meta = codec::data_key(P, codec::KIND_HASH_META, b"h");
        assert_eq!(user_key_of(&meta), Some(b"h".to_vec()));
        let elem = codec::elem_key(P, codec::KIND_HASH_FLD, b"h", b"f");
        assert_eq!(user_key_of(&elem), None);
        let idx = codec::expire_index_key(P, 9, &meta);
        assert_eq!(user_key_of(&idx), None);
        // No slot prefix at all: not our layout.
        assert_eq!(user_key_of(b"nope"), None);
    }

    #[test]
    fn collect_user_keys_pages_by_count_with_resume() {
        let (_guard, shared) = shared();
        for i in 0..5u8 {
            put_raw(&shared, format!("k{}", i).as_bytes(), b"v");
        }
        let p1 = collect_user_keys(&shared.store, P, P, None, None, 2).expect("page 1");
        assert_eq!(p1.keys, vec![b"k0".to_vec(), b"k1".to_vec()]);
        assert!(!p1.next.is_empty());
        let p2 = collect_user_keys(&shared.store, P, &p1.next, None, None, 2).expect("page 2");
        assert_eq!(p2.keys, vec![b"k2".to_vec(), b"k3".to_vec()]);
        let p3 = collect_user_keys(&shared.store, P, &p2.next, None, None, 2).expect("page 3");
        assert_eq!(p3.keys, vec![b"k4".to_vec()]);
        assert!(p3.next.is_empty(), "iteration finished");
    }

    #[test]
    fn collect_user_keys_skips_internal_records_and_filters() {
        let (_guard, shared) = shared();
        put_raw(&shared, b"user", b"v");
        let meta = codec::data_key(P, codec::KIND_ZSET_META, b"z");
        crate::store::ops::batch_write(&shared.store, {
            let mut b = rocksdb::WriteBatch::default();
            b.put(&meta, codec::encode_envelope(0, b"m"));
            b.put(codec::elem_key(P, codec::KIND_ZSET_MEMBER, b"z", b"s"), b"");
            b.put(codec::expire_index_key(P, 5, &meta), b"");
            b
        })
        .expect("batch");
        let all = all_user_keys(&shared.store, P, None).expect("all keys");
        // Physical order: typed META records (kind byte < 'u') sort before
        // the raw string "user".
        assert_eq!(all, vec![b"z".to_vec(), b"user".to_vec()]);
        let only_u = all_user_keys(&shared.store, P, Some(b"u*")).expect("u* keys");
        assert_eq!(only_u, vec![b"user".to_vec()]);
        let only_z = all_user_keys(&shared.store, P, Some(b"z")).expect("z keys");
        assert_eq!(only_z, vec![b"z".to_vec()]);
    }

    #[test]
    fn collect_user_keys_filters_by_type() {
        let (_guard, shared) = shared();
        put_raw(&shared, b"str", b"v");
        let meta = codec::data_key(P, codec::KIND_ZSET_META, b"z");
        crate::store::ops::batch_write(&shared.store, {
            let mut b = rocksdb::WriteBatch::default();
            b.put(&meta, codec::encode_envelope(0, b"m"));
            b
        })
        .expect("batch");
        // Raw strings and META records both type-filter; the value is
        // case-insensitive and unfiltered scans still see everything.
        let strings =
            collect_user_keys(&shared.store, P, P, None, Some(b"string"), 10).expect("string page");
        assert_eq!(strings.keys, vec![b"str".to_vec()]);
        let zsets =
            collect_user_keys(&shared.store, P, P, None, Some(b"ZSET"), 10).expect("zset page");
        assert_eq!(zsets.keys, vec![b"z".to_vec()]);
        let none =
            collect_user_keys(&shared.store, P, P, None, Some(b"hash"), 10).expect("hash page");
        assert!(none.keys.is_empty() && none.next.is_empty());
        // Known names accepted, unknown ones rejected (SCAN TYPE syntax).
        assert!(is_scan_type_name(b"hash") && is_scan_type_name(b"rejson-rl"));
        assert!(!is_scan_type_name(b"bogus") && !is_scan_type_name(b"none"));
    }

    #[test]
    fn random_user_key_wraps_to_some_key() {
        let (_guard, shared) = shared();
        // Empty database: nothing anywhere.
        assert_eq!(random_user_key(&shared.store).expect("random"), None);
        put_raw(&shared, b"only", b"v");
        // With a single key, any start slot must wrap around to it.
        for _ in 0..32 {
            assert_eq!(
                random_user_key(&shared.store).expect("random"),
                Some(b"only".to_vec())
            );
        }
    }

    /// The storage backends are `Result`-typed: "no data" (empty page,
    /// empty key list, no random key) is always an `Ok` variant, so a
    /// caller can map `Ok(None)`/`Ok(empty)` to a normal reply and any
    /// `Err` to `-ERR` without ambiguity. Storage iterator failures
    /// cannot be forced in-process here; the plumbing is asserted by
    /// these signatures plus the `.expect` unwraps above.
    #[test]
    fn empty_results_are_ok_not_fabricated() {
        let (_guard, shared) = shared();
        let page = collect_user_keys(&shared.store, P, P, None, None, 10).expect("empty scan page");
        assert!(page.keys.is_empty() && page.next.is_empty());
        let all = all_user_keys(&shared.store, P, None).expect("empty key list");
        assert!(all.is_empty());
        assert_eq!(random_user_key(&shared.store).expect("no random key"), None);
    }
}
