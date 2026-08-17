//! Sorted-set storage ops: dual records per member (name `zset_ds` mirrors
//! `set_ds`/`hash_ds`). Physical layout, one user key per zset:
//!
//! ```text
//! meta   = data_key(prefix, KIND_ZSET_META, key)
//!          value = envelope ++ LEB128(count)
//! member = elem_key(prefix, KIND_ZSET_MEMBER, key, member)
//!          value = 8-byte big-endian SORTABLE score (existence + lookup)
//! score  = elem_key(prefix, KIND_ZSET_SCORE, key, sortable(score) ++ member)
//!          value = b"" (the ordered index record)
//! ```
//!
//! Score records ARE the ordering: ascending physical order = ascending
//! score, member bytes within equal scores, so ZRANGE-style windows are
//! plain forward scans over `KIND_ZSET_SCORE`. Member records give O(1)
//! score lookup; every write maintains both (command layer, later phase).
//! Iteration windows are verified by DECODING each key (`is_score_record`)
//! -- kind, user key and the 8-byte sortable prefix -- instead of byte
//! range heuristics, so no member/score combination can leak in or drop
//! out (a +inf score's suffix starts `FF..FF` exactly like the naive
//! upper bound `[0xFF; 8]`, which is why bounds are not used here).

use rocksdb::WriteBatch;

use crate::ds::codec::{self, KIND_ZSET_MEMBER, KIND_ZSET_META, KIND_ZSET_SCORE, ZSET_FAMILY};
use crate::ds::expire;
use crate::store::{ops, Store};

/// Sign bit of an f64's bit pattern; the pivot of the sortable encoding.
const SIGN_BIT: u64 = 0x8000_0000_0000_0000;

/// Root counters of one zset: TTL plus member count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSetMeta {
    pub expire_ms: u64,
    pub count: u64,
}

/// Result of reading a zset's meta record.
#[derive(Debug, PartialEq, Eq)]
pub enum ZSetMetaRead {
    /// No meta record: the zset does not exist.
    Missing,
    /// Live zset: absolute expiry (0 = none) and member count.
    Present(ZSetMeta),
    /// Expired: the whole family was just purged.
    Purged,
    /// Store error.
    Failed(String),
}

/// Meta/root physical key of a zset.
pub fn meta_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_ZSET_META, key)
}

/// Read + lazily expire one zset's meta. Wrong-type detection is the
/// command layer's job (via `keys_core::resolve`).
pub fn read_meta(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> ZSetMetaRead {
    let root = meta_key(prefix, key);
    let val = match ops::get_physical(store, &root) {
        Err(e) => return ZSetMetaRead::Failed(e),
        Ok(None) => return ZSetMetaRead::Missing,
        Ok(Some(v)) => v,
    };
    let (expire_ms, payload) = codec::decode_envelope(&val);
    if expire::is_expired(expire_ms, now) {
        return if expire::purge_if_expired(store, prefix, ZSET_FAMILY, key, now) {
            ZSetMetaRead::Purged
        } else {
            ZSetMetaRead::Failed("purge failed".to_string())
        };
    }
    ZSetMetaRead::Present(ZSetMeta {
        expire_ms,
        count: codec::decode_count(payload),
    })
}

/// Put the meta record into `batch`, keeping the TTL envelope.
pub fn write_meta(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], meta: &ZSetMeta) {
    batch.put(
        meta_key(prefix, key),
        codec::encode_envelope(meta.expire_ms, &codec::encode_count(meta.count)),
    );
}

/// Batch entries wiping the whole zset family and its TTL index entry.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64) {
    expire::family_delete_entries(batch, prefix, ZSET_FAMILY, key, expire_ms);
}

/// Score sort key (Redis trick): map an f64 onto u64 byte order so
/// lexicographic comparison equals numeric comparison. Negatives flip
/// every bit; non-negatives set the sign bit. Monotonic for all finite
/// values and +-inf. NaN is REJECTED at the command layer (its pattern
/// would order between the negatives and -0.0). Note `-0.0` sorts
/// strictly before `+0.0` (distinct bits, distinct sortables).
pub fn score_sortable(score: f64) -> u64 {
    let bits = score.to_bits();
    if bits & SIGN_BIT != 0 {
        !bits
    } else {
        bits | SIGN_BIT
    }
}

/// Inverse of [`score_sortable`]; bit-exact for every sortable produced
/// by it (including both zeros and the infinities).
pub fn sortable_score(bits: u64) -> f64 {
    if bits & SIGN_BIT != 0 {
        f64::from_bits(bits & !SIGN_BIT)
    } else {
        f64::from_bits(!bits)
    }
}

/// Physical key of one member's score-lookup record.
pub fn member_key(prefix: &[u8], key: &[u8], member: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_ZSET_MEMBER, key, member)
}

/// Score-record suffix: 8-byte sortable score ++ raw member bytes.
fn score_suffix(score: f64, member: &[u8]) -> Vec<u8> {
    let mut out = score_sortable(score).to_be_bytes().to_vec();
    out.extend_from_slice(member);
    out
}

/// Physical key of one ordered score record, from raw sortable bytes.
pub fn score_key(prefix: &[u8], key: &[u8], score_bytes: &[u8; 8], member: &[u8]) -> Vec<u8> {
    codec::elem_key(
        prefix,
        KIND_ZSET_SCORE,
        key,
        &[score_bytes.as_slice(), member].concat(),
    )
}

/// Score-record key from a raw suffix (sortable ++ member). Bounds built
/// by later phases pass score+member bytes straight through; an EMPTY
/// suffix is the window start (sorts before every 8+-byte suffix).
pub fn score_key_from_suffix(prefix: &[u8], key: &[u8], suffix: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_ZSET_SCORE, key, suffix)
}

/// One member's score; `Ok(None)` = member absent (or a corrupt short
/// record -- writers always store exactly 8 bytes).
pub fn member_score(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    member: &[u8],
) -> Result<Option<f64>, String> {
    let val = ops::get_physical(store, &member_key(prefix, key, member))?;
    Ok(val
        .as_deref()
        .and_then(|v| v.get(..8))
        .and_then(|b| b.try_into().ok())
        .map(u64::from_be_bytes)
        .map(sortable_score))
}

/// Put the member (score-lookup) record: value = 8-byte sortable score.
pub fn put_member(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], member: &[u8], score: f64) {
    batch.put(
        member_key(prefix, key, member),
        score_sortable(score).to_be_bytes(),
    );
}

/// Put the ordered (empty-valued) score record for `member` at `score`.
pub fn put_scored(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], score: f64, member: &[u8]) {
    batch.put(
        score_key_from_suffix(prefix, key, &score_suffix(score, member)),
        b"",
    );
}

/// Drop one member (score-lookup) record.
pub fn del_member(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], member: &[u8]) {
    batch.delete(member_key(prefix, key, member));
}

/// Drop one ordered score record (`score` must equal the stored score).
pub fn del_scored(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], score: f64, member: &[u8]) {
    batch.delete(score_key_from_suffix(
        prefix,
        key,
        &score_suffix(score, member),
    ));
}

/// Iteration window check: `Some(suffix)` when `physical` is a score
/// record of `key` -- kind, user key AND an 8+-byte suffix verified by
/// DECODING the key (exact; no byte-range heuristics). Ascending scans
/// treat `None` as "iteration left the window" and stop.
pub fn is_score_record<'a>(physical: &'a [u8], prefix_len: usize, key: &[u8]) -> Option<&'a [u8]> {
    let (kind, user_key, suffix) = codec::decode_data_key(physical, prefix_len)?;
    if kind != KIND_ZSET_SCORE || user_key != key || suffix.len() < 8 {
        return None;
    }
    Some(suffix)
}

/// Iterate score records ASCENDING from `from_suffix` (empty = window
/// start); `excl_from` skips a leading record whose suffix equals
/// `from_suffix`. `f(member, score)` returns `false` to stop early;
/// iteration also ends once keys leave `key`'s `KIND_ZSET_SCORE` window.
pub fn for_each_scored(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    from_suffix: &[u8],
    excl_from: bool,
    f: &mut dyn FnMut(&[u8], f64) -> bool,
) -> Result<(), String> {
    let start = score_key_from_suffix(prefix, key, from_suffix);
    ops::for_each_from(store, &start, excl_from, &mut |k, _| {
        let Some(suffix) = is_score_record(k, prefix.len(), key) else {
            return false; // left the window
        };
        let score = sortable_score(u64::from_be_bytes(
            suffix[..8].try_into().expect(">= 8 bytes checked"),
        ));
        f(&suffix[8..], score)
    })
}

/// Number of score records with physical keys STRICTLY before
/// `score_key_from_suffix(suffix)` -- the 0-based rank of that record
/// (ZRANK). `suffix` = sortable ++ member of the queried member.
pub fn count_before(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    suffix: &[u8],
) -> Result<u64, String> {
    let bound = score_key_from_suffix(prefix, key, suffix);
    let mut count = 0u64;
    ops::for_each_from(
        store,
        &score_key_from_suffix(prefix, key, b""),
        false,
        &mut |k, _| match is_score_record(k, prefix.len(), key) {
            Some(_) if k < bound.as_slice() => {
                count += 1;
                true
            }
            _ => false, // reached (or passed) the bound, or left the window
        },
    )?;
    Ok(count)
}
