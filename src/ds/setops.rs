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

/// Error text for RENAME between a lite stream name and an ordinary key:
/// the two live in different physical windows (parent slot vs. routed
/// slot), so no single-window move can carry one onto the other.
pub const RENAME_MIXED_ERROR: &str =
    "ERR RENAME source and destination must both be lite stream names or both be ordinary keys";

/// Do all keys hash to the same cluster slot? Slot is computed from the
/// hash tag exactly like routing (`hash::hash_tag` + CRC16).
pub fn same_slot(keys: &[Vec<u8>]) -> bool {
    let mut slots = keys.iter().map(|k| slot_of(k));
    match slots.next() {
        None => true, // no keys: vacuously same
        Some(first) => slots.all(|s| s == first),
    }
}

/// Routing slot of one ordinary key (hash tag + CRC16).
fn slot_of(key: &[u8]) -> u16 {
    crate::hash::slot_with_prefix(crate::hash::hash_tag(key)).0
}

/// RENAME/RENAMENX window rule (batch-14 closure, form A): both operands
/// must address ONE physical window. A valid lite stream name addresses
/// its PARENT slot (`lite::model::stream_key_target`), so `parent/a ->
/// parent/b` is a same-window move although the full names hash apart,
/// `p1/a -> p2/b` is CROSSSLOT, and a stream name next to an ordinary
/// key never shares a window.
#[derive(Debug, PartialEq, Eq)]
pub enum RenameSlot {
    /// Both ordinary keys of one hash-tag slot (the routed prefix).
    Ordinary,
    /// Both streams of one parent: the shared parent slot prefix.
    Stream(Vec<u8>),
    CrossSlot,
    Mixed,
}

pub fn rename_slot(src: &[u8], dst: &[u8]) -> RenameSlot {
    match (
        crate::lite::model::stream_key_target(src),
        crate::lite::model::stream_key_target(dst),
    ) {
        (Some(a), Some(b)) => {
            if a == b {
                RenameSlot::Stream(a)
            } else {
                RenameSlot::CrossSlot
            }
        }
        (None, None) => {
            if slot_of(src) == slot_of(dst) {
                RenameSlot::Ordinary
            } else {
                RenameSlot::CrossSlot
            }
        }
        _ => RenameSlot::Mixed,
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
            match set_ds::collect_members(store, prefix, key, None, None, 0) {
                Ok(page) => MembersRead::Members(page.members.into_iter().collect()),
                Err(e) => MembersRead::Failed(e),
            }
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
            expire::family_delete_entries(batch, Some(store), prefix, family, key, expire_ms);
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
    fn rename_slot_windows() {
        // Two streams of one parent share the PARENT slot window even
        // though the full names hash apart (batch-14 closure, form A).
        let parent = crate::lite::model::stream_key_target(b"orders/q0").unwrap();
        assert_eq!(rename_slot(b"orders/q0", b"orders/q9"), RenameSlot::Stream(parent));
        assert_eq!(rename_slot(b"p1/q0", b"p2/q0"), RenameSlot::CrossSlot);
        assert_eq!(rename_slot(b"p1/q0", b"plain"), RenameSlot::Mixed);
        assert_eq!(rename_slot(b"plain", b"p1/q0"), RenameSlot::Mixed);
        // Ordinary keys: the routed hash-tag slot decides, as before.
        assert_eq!(rename_slot(b"{x}a", b"{x}b"), RenameSlot::Ordinary);
        assert_eq!(rename_slot(b"aa", b"bb"), RenameSlot::CrossSlot);
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
