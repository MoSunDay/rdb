//! Pure set algebra over stored sets: SUNION/SINTER/SDIFF and their
//! *STORE twins read each operand's members and compute the result in
//! memory (sets are bounded by usage). Results are returned SORTED --
//! Redis leaves the order unspecified, and a deterministic order keeps
//! replies testable.
//!
//! Multi-key cluster rule: every operand must hash to the SAME slot
//! (the RESP layer derives one slot prefix from the FIRST key only);
//! callers enforce [`same_slot`] and reply [`CROSSSLOT_ERROR`] otherwise.

use std::collections::HashSet;

use rocksdb::WriteBatch;

use crate::command::keys_core::{self, KeyState};
use crate::ds::codec::{self, KIND_SET_META};
use crate::ds::expire;
use crate::ds::set_ds;
use crate::resp::codec::append_error;
use crate::store::Store;

/// Error text for keys hashing to different slots (Redis cluster wording).
pub const CROSSSLOT_ERROR: &str = "ERR CROSSSLOT Keys in request don't hash to the same slot";

/// Do all keys hash to the same cluster slot? Slot is computed from the
/// hash tag exactly like routing (`hash::hash_tag` + CRC16).
pub fn same_slot(keys: &[Vec<u8>]) -> bool {
    let mut slots = keys
        .iter()
        .map(|k| crate::hash::slot_with_prefix(crate::hash::hash_tag(k)).0);
    match slots.next() {
        None => true, // no keys: vacuously same
        Some(first) => slots.all(|s| s == first),
    }
}

/// Entry-point guard for multi-key commands (MGET/MSET/DEL/EXISTS/RENAME
/// family): unless every key hashes to one slot, append the CROSSSLOT
/// error to `out` and return false so the caller can stop right there.
pub fn require_same_slot(out: &mut Vec<u8>, keys: &[Vec<u8>]) -> bool {
    if same_slot(keys) {
        return true;
    }
    append_error(out, CROSSSLOT_ERROR);
    false
}

/// One operand's members; `WrongType` = key exists but is not a set.
#[derive(Debug, PartialEq)]
pub enum MembersRead {
    Missing,
    WrongType,
    Members(HashSet<Vec<u8>>),
    Failed(String),
}

/// Resolve one operand: lazily expires, then reads every member. Reads
/// via `keys_core::resolve` first so raw strings and other families are
/// reported as `WrongType` instead of silently reading as empty.
pub fn read_members(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> MembersRead {
    match keys_core::resolve(store, prefix, key, now) {
        KeyState::Missing => MembersRead::Missing,
        KeyState::RawString { .. } => MembersRead::WrongType,
        KeyState::Enveloped { kind, .. } if kind != KIND_SET_META => MembersRead::WrongType,
        KeyState::Enveloped { .. } => {
            let page = set_ds::collect_members(store, prefix, key, None, None, 0);
            MembersRead::Members(page.members.into_iter().collect())
        }
    }
}

/// Union of all operands, sorted. Empty input = empty result.
pub fn union_all(sets: &[HashSet<Vec<u8>>]) -> Vec<Vec<u8>> {
    let mut acc: HashSet<Vec<u8>> = HashSet::new();
    for s in sets {
        acc.extend(s.iter().cloned());
    }
    let mut out: Vec<Vec<u8>> = acc.into_iter().collect();
    out.sort();
    out
}

/// Intersection of all operands, sorted; empty input = empty result.
pub fn intersect_all(sets: &[HashSet<Vec<u8>>]) -> Vec<Vec<u8>> {
    // Seed from the smallest operand: the intersection can never be bigger
    // than any operand, so this bounds the membership checks.
    let Some(seed) = sets.iter().min_by_key(|s| s.len()) else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = seed
        .iter()
        .filter(|m| sets.iter().all(|s| s.contains(*m)))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Difference: first operand minus every later one, sorted.
pub fn diff_all(sets: &[HashSet<Vec<u8>>]) -> Vec<Vec<u8>> {
    let Some((first, rest)) = sets.split_first() else {
        return Vec::new();
    };
    let mut out: Vec<Vec<u8>> = first
        .iter()
        .filter(|m| !rest.iter().any(|s| s.contains(*m)))
        .cloned()
        .collect();
    out.sort();
    out
}

/// Overwrite `key` with `members` inside `batch`: wipe whatever family the
/// destination held (any type, plus its TTL index entry) and write a fresh
/// no-TTL set meta + member records. Returns the new cardinality.
/// Wipe whatever currently occupies `key` (raw string or any enveloped
/// family, including its TTL index entry). Shared by [`store_set`] and
/// the empty-result path of the STORE commands.
pub fn store_clear(batch: &mut WriteBatch, store: &Store, prefix: &[u8], key: &[u8], now: u64) {
    match keys_core::resolve(store, prefix, key, now) {
        KeyState::Missing => {}
        KeyState::RawString { .. } => {
            batch.delete(codec::string_key(prefix, key));
        }
        KeyState::Enveloped {
            kind, expire_ms, ..
        } => {
            let family = codec::family_of(kind).unwrap_or(codec::SET_FAMILY);
            expire::family_delete_entries(batch, prefix, family, key, expire_ms);
        }
    }
}

/// Overwrite `key` with exactly `members` (no TTL): wipe any previous
/// value of ANY type, then write fresh meta + members. Reply count is the
/// return value.
pub fn store_set(
    batch: &mut WriteBatch,
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    members: &[Vec<u8>],
    now: u64,
) -> usize {
    store_clear(batch, store, prefix, key, now);
    set_ds::write_meta(batch, prefix, key, 0, members.len() as u64);
    for m in members {
        batch.put(set_ds::member_key(prefix, key, m), b"");
    }
    members.len()
}

/// Convenience for tests: are two member lists equal as sets?
#[cfg(test)]
pub fn set_eq(a: &[Vec<u8>], b: &[Vec<u8>]) -> bool {
    let (x, y): (HashSet<_>, HashSet<_>) = (a.iter().collect(), b.iter().collect());
    x == y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hs(members: &[&[u8]]) -> HashSet<Vec<u8>> {
        members.iter().map(|m| m.to_vec()).collect()
    }

    #[test]
    fn same_slot_hash_tags_and_whole_keys() {
        // Same tag -> same slot even with different prefixes/suffixes.
        assert!(same_slot(&[b"{t}a".to_vec(), b"{t}b".to_vec()]));
        // Identical keys trivially pass; empty passes vacuously.
        assert!(same_slot(&[b"k".to_vec(), b"k".to_vec()]));
        assert!(same_slot(&[]));
        // Different tags normally differ (CRC16 collision-free picks).
        assert!(!same_slot(&[b"one".to_vec(), b"two".to_vec()]));
        // Same slot by explicit known collision-free pair: "foo" & "bar"
        // hash differently; verified below via slot numbers.
        let (s1, _) = crate::hash::slot_with_prefix(b"foo");
        let (s2, _) = crate::hash::slot_with_prefix(b"bar");
        assert_ne!(s1, s2);
        assert!(!same_slot(&[b"foo".to_vec(), b"bar".to_vec()]));
        assert!(same_slot(&[b"{x}foo".to_vec(), b"{x}bar".to_vec()]));
    }

    #[test]
    fn algebra_union_intersect_diff() {
        let a = hs(&[b"a", b"b", b"c"]);
        let b = hs(&[b"b", b"c", b"d"]);
        let c = hs(&[b"c"]);
        assert_eq!(
            union_all(&[a.clone(), b.clone()]),
            vec![b"a", b"b", b"c", b"d"]
        );
        assert_eq!(intersect_all(&[a.clone(), b.clone()]), vec![b"b", b"c"]);
        assert_eq!(intersect_all(&[a.clone(), b.clone(), c]), vec![b"c"]);
        assert_eq!(diff_all(&[a.clone(), b.clone()]), vec![b"a"]);
        assert_eq!(diff_all(std::slice::from_ref(&a)), vec![b"a", b"b", b"c"]);
        assert!(union_all(&[]).is_empty());
        assert!(intersect_all(&[]).is_empty());
        assert!(diff_all(&[]).is_empty());
        // Duplicate operands behave (SUNION s s == s).
        assert_eq!(union_all(&[a.clone(), a.clone()]), vec![b"a", b"b", b"c"]);
        assert_eq!(intersect_all(&[a.clone(), a]), vec![b"a", b"b", b"c"]);
    }

    #[test]
    fn intersect_seeds_from_smallest_operand() {
        let big: HashSet<Vec<u8>> = (0..100u32).map(|i| i.to_string().into_bytes()).collect();
        let small = hs(&[b"7", b"999"]);
        assert_eq!(intersect_all(&[big, small]), vec![b"7"]);
    }
}
