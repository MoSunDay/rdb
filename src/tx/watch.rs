//! WATCH value hashing: a compact fingerprint of EVERYTHING stored for
//! one user key, across all physical layouts (raw string record + every
//! typed-kind family range).
//!
//! A change to any byte of the key's state (value, TTL envelope, family
//! element, purge) changes the hash, so EXEC-time re-hashing under the
//! latches detects every intervening write -- including lazy expire
//! purges performed by OTHER connections' read paths.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::ds::codec;
use crate::store::key_upper_bound;
use crate::store::ops::{for_each_from, get_physical};
use crate::store::Store;

/// Bounded family scan: iterate [lower, upper) feeding the callback;
/// the callback returns `false` to stop early (unused by the hasher,
/// which always drains the range).
///
/// User-data kind range: KIND_STRING_TTL (0x01) ..= KIND_VECTORSET_ELEM
/// (0x12). The raw-string layout (0x00) is hashed separately below; the
/// expire index (0xFD) is derived state (its contents follow the data
/// records) and is covered transitively.
const USER_KINDS: std::ops::RangeInclusive<u8> =
    codec::KIND_STRING_TTL..=codec::KIND_VECTORSET_ELEM;

/// Hash every physical byte stored for `key` under `prefix`.
///
/// Present/absent is hashed distinctly from bytes (a `write_u8` marker
/// before each value) so an empty value can never collide with no value.
pub fn value_hash(store: &Store, prefix: &[u8], key: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();

    // Raw string layout: <prefix><key> directly.
    let raw = codec::string_key(prefix, key);
    match get_physical(store, &raw) {
        Ok(Some(v)) => {
            h.write_u8(1);
            v.hash(&mut h);
        }
        _ => h.write_u8(0),
    }

    // Typed layouts: every kind's family range [data_key, upper_bound).
    for kind in USER_KINDS {
        h.write_u8(kind);
        let lower = codec::data_key(prefix, kind, key);
        let upper = key_upper_bound(&lower).unwrap_or_default();
        if let Err(e) = for_each_from(store, &lower, false, &mut |k, v| {
            if !upper.is_empty() && k >= upper.as_slice() {
                return false; // past the family range: stop
            }
            k.hash(&mut h);
            v.hash(&mut h);
            true
        }) {
            // A read error cannot be distinguished from absence; fold the
            // error text in so it still differs from a clean read.
            h.write_u8(0xFF);
            e.hash(&mut h);
        }
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ds::codec::{KIND_HASH_FLD, KIND_HASH_META, KIND_STRING_TTL, KIND_ZSET_MEMBER};
    use crate::state::testutil;

    fn shared() -> crate::state::Shared {
        testutil::shared_with(crate::conf::Config::default())
    }

    #[test]
    fn hash_tracks_all_layouts() {
        let sh = shared();
        let p = b"42/" as &[u8];
        let k = b"key" as &[u8];
        let h0 = value_hash(&sh.store, p, k);

        // raw string
        sh.store.db.put(codec::string_key(p, k), b"v").unwrap();
        let h1 = value_hash(&sh.store, p, k);
        assert_ne!(h0, h1);

        // same bytes re-read: stable
        assert_eq!(h1, value_hash(&sh.store, p, k));

        // string TTL envelope (family root)
        sh.store
            .db
            .put(codec::data_key(p, KIND_STRING_TTL, k), b"meta")
            .unwrap();
        let h2 = value_hash(&sh.store, p, k);
        assert_ne!(h1, h2);

        // hash meta + element
        sh.store
            .db
            .put(codec::data_key(p, KIND_HASH_META, k), b"m")
            .unwrap();
        sh.store
            .db
            .put(codec::elem_key(p, KIND_HASH_FLD, k, b"f"), b"1")
            .unwrap();
        let h3 = value_hash(&sh.store, p, k);
        assert_ne!(h2, h3);

        // unrelated keyspace bytes do not affect the hash
        sh.store
            .db
            .put(codec::elem_key(p, KIND_ZSET_MEMBER, b"other", b"m"), b"x")
            .unwrap();
        assert_eq!(h3, value_hash(&sh.store, p, k));

        // deleting back to empty restores the original absent-state hash
        sh.store.db.delete(codec::string_key(p, k)).unwrap();
        sh.store
            .db
            .delete(codec::data_key(p, KIND_STRING_TTL, k))
            .unwrap();
        sh.store
            .db
            .delete(codec::data_key(p, KIND_HASH_META, k))
            .unwrap();
        sh.store
            .db
            .delete(codec::elem_key(p, KIND_HASH_FLD, k, b"f"))
            .unwrap();
        assert_eq!(h0, value_hash(&sh.store, p, k));
    }
}
