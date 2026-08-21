//! Derived-key codec for every typed record under a slot prefix.
//!
//! Physical layouts (all big-endian fixed ints, LEB128 for the envelope):
//!
//! ```text
//! data key   = <slot_prefix> ++ <kind:u8> ++ <key_len:u32 BE> ++ <user_key>
//! elem key   = data key ++ <element suffix bytes>
//! expire idx = <slot_prefix> ++ 0xFD ++ <expire_ms:u64 BE> ++ <data key from kind on>
//! value      = <expire_ms varuint (LEB128, 0 = no expiry)> ++ <payload>   (kinds != 0x00)
//! ```
//!
//! EXCEPTION -- kind 0x00 (raw STRING): stored as `<slot_prefix> ++ <user_key>`
//! with a bare value, NO envelope. That keeps SET/GET byte-identical to the
//! pre-ttl layout, so raw string keys are indistinguishable from legacy
//! databases (intentional; old dev DBs keep working until their first EXPIRE
//! migrates the record to kind 0x01).
//!
//! Kind families are numerically adjacent (e.g. HASH 0x02..=0x03); deletes
//! use ONE RANGE PER KIND (`family_delete_ranges`) -- a single family-wide
//! span would swallow other keys' records since the kind byte sorts first.

use crate::store::key_upper_bound;

/// Kind byte registry -- the single source of truth for record types.
pub type CodecFamily = (u8, u8);

pub const KIND_STRING: u8 = 0x00;
pub const KIND_STRING_TTL: u8 = 0x01;
pub const KIND_HASH_META: u8 = 0x02;
pub const KIND_HASH_FLD: u8 = 0x03;
pub const KIND_LIST_META: u8 = 0x04;
pub const KIND_LIST_L: u8 = 0x05;
pub const KIND_LIST_R: u8 = 0x06;
pub const KIND_SET_META: u8 = 0x07;
pub const KIND_SET_MEMBER: u8 = 0x08;
pub const KIND_ZSET_META: u8 = 0x09;
pub const KIND_ZSET_MEMBER: u8 = 0x0A;
pub const KIND_ZSET_SCORE: u8 = 0x0B;
pub const KIND_STREAM_META: u8 = 0x0C;
pub const KIND_STREAM_ENTRY: u8 = 0x0D;
pub const KIND_STREAM_GROUP: u8 = 0x0E;
pub const KIND_STREAM_PEND: u8 = 0x0F;
pub const KIND_JSON: u8 = 0x10;
pub const KIND_VECTORSET_META: u8 = 0x11;
pub const KIND_VECTORSET_ELEM: u8 = 0x12;
/// FT.* full-text/search-engine family (`crate::search`): index meta
/// (schema + corpus stats), per-doc records, inverted postings, term
/// statistics, ANN centroid table and ANN partition postings. One
/// family so a single lazy-purge/TTL wipe covers text + vector parts.
pub const KIND_SEARCH_META: u8 = 0x13;
pub const KIND_SEARCH_DOC: u8 = 0x14;
pub const KIND_SEARCH_POSTING: u8 = 0x15;
pub const KIND_SEARCH_TERMSTAT: u8 = 0x16;
pub const KIND_ANN_CENTROID: u8 = 0x17;
pub const KIND_ANN_POSTING: u8 = 0x18;
/// Never a user-visible type: the active-expiration index record.
pub const KIND_EXPIRE_INDEX: u8 = 0xFD;

/// First..=last kind of each family; `(first, last)` (spec calls this the
/// "kind family"). STRING covers only the enveloped 0x01 record -- raw 0x00
/// keys live at `<prefix><key>` and are deleted via [`string_key`].
pub const STRING_FAMILY: CodecFamily = (KIND_STRING_TTL, KIND_STRING_TTL);
pub const HASH_FAMILY: CodecFamily = (KIND_HASH_META, KIND_HASH_FLD);
pub const LIST_FAMILY: CodecFamily = (KIND_LIST_META, KIND_LIST_R);
pub const SET_FAMILY: CodecFamily = (KIND_SET_META, KIND_SET_MEMBER);
pub const ZSET_FAMILY: CodecFamily = (KIND_ZSET_META, KIND_ZSET_SCORE);
pub const STREAM_FAMILY: CodecFamily = (KIND_STREAM_META, KIND_STREAM_PEND);
pub const JSON_FAMILY: CodecFamily = (KIND_JSON, KIND_JSON);
pub const VECTORSET_FAMILY: CodecFamily = (KIND_VECTORSET_META, KIND_VECTORSET_ELEM);
pub const SEARCH_FAMILY: CodecFamily = (KIND_SEARCH_META, KIND_ANN_POSTING);

/// Meta/root kinds a user key can exist under (one record = key "exists").
pub const META_KINDS: [u8; 9] = [
    KIND_STRING_TTL,
    KIND_HASH_META,
    KIND_LIST_META,
    KIND_SET_META,
    KIND_ZSET_META,
    KIND_STREAM_META,
    KIND_JSON,
    KIND_VECTORSET_META,
    KIND_SEARCH_META,
];

/// `true` for kinds that represent a whole user key (raw strings are the
/// other user-key shape and are recognized by [`classify`] instead).
pub fn is_user_key_kind(kind: u8) -> bool {
    META_KINDS.contains(&kind)
}

pub fn meta_kinds() -> &'static [u8] {
    &META_KINDS
}

/// Family span containing `kind`; `None` for raw strings and unknown bytes.
pub fn family_of(kind: u8) -> Option<CodecFamily> {
    let family = match kind {
        KIND_STRING_TTL => STRING_FAMILY,
        KIND_HASH_META | KIND_HASH_FLD => HASH_FAMILY,
        KIND_LIST_META | KIND_LIST_L | KIND_LIST_R => LIST_FAMILY,
        KIND_SET_META | KIND_SET_MEMBER => SET_FAMILY,
        KIND_ZSET_META | KIND_ZSET_MEMBER | KIND_ZSET_SCORE => ZSET_FAMILY,
        KIND_STREAM_META | KIND_STREAM_ENTRY | KIND_STREAM_GROUP | KIND_STREAM_PEND => {
            STREAM_FAMILY
        }
        KIND_JSON => JSON_FAMILY,
        KIND_VECTORSET_META | KIND_VECTORSET_ELEM => VECTORSET_FAMILY,
        KIND_SEARCH_META | KIND_SEARCH_DOC | KIND_SEARCH_POSTING | KIND_SEARCH_TERMSTAT
        | KIND_ANN_CENTROID | KIND_ANN_POSTING => SEARCH_FAMILY,
        _ => return None,
    };
    Some(family)
}

/// Raw string physical key: kind 0x00 stores `<prefix> ++ <key>` directly.
pub fn string_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    [prefix, key].concat()
}

/// Meta/root physical key for a typed record.
pub fn data_key(prefix: &[u8], kind: u8, key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + 5 + key.len());
    out.extend_from_slice(prefix);
    out.push(kind);
    out.extend_from_slice(&(key.len() as u32).to_be_bytes());
    out.extend_from_slice(key);
    out
}

/// Element physical key: data key plus type-specific suffix bytes.
pub fn elem_key(prefix: &[u8], kind: u8, key: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut out = data_key(prefix, kind, key);
    out.extend_from_slice(suffix);
    out
}

/// Inverse of [`data_key`]/[`elem_key`]. `prefix_len` is the slot prefix
/// length -- the caller always iterates inside a known slot, so the prefix
/// is never guessed. Returns `None` for raw/expired layouts or malformed
/// lengths.
pub fn decode_data_key(physical: &[u8], prefix_len: usize) -> Option<(u8, Vec<u8>, &[u8])> {
    let body = physical.get(prefix_len..)?;
    let kind = *body.first()?;
    family_of(kind)?; // reject 0x00 raw, 0xFD index and unknown bytes
    let len = u32::from_be_bytes(body.get(1..5)?.try_into().ok()?) as usize;
    let key = body.get(5..5 + len)?;
    Some((kind, key.to_vec(), body.get(5 + len..).unwrap_or(&[])))
}

/// Expire-index physical key; `data_key` is the FULL physical data key
/// (prefix included) -- its prefix is stripped because it is repeated in
/// `prefix` here, keeping index entries self-contained per slot.
pub fn expire_index_key(prefix: &[u8], expire_ms: u64, data_key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + 9 + data_key.len());
    out.extend_from_slice(prefix);
    out.push(KIND_EXPIRE_INDEX);
    out.extend_from_slice(&expire_ms.to_be_bytes());
    out.extend_from_slice(&data_key[prefix.len()..]);
    out
}

/// Inverse of [`expire_index_key`] -> `(expire_ms, data key body from kind on)`.
pub fn decode_expire_index_key(physical: &[u8], prefix_len: usize) -> Option<(u64, Vec<u8>)> {
    let body = physical.get(prefix_len..)?;
    if body.first() != Some(&KIND_EXPIRE_INDEX) {
        return None;
    }
    let expire = u64::from_be_bytes(body.get(1..9)?.try_into().ok()?);
    Some((expire, body.get(9..)?.to_vec()))
}

/// Delete ranges covering every record of `key` under `family`: ONE RANGE
/// PER KIND, each confined by `key_upper_bound` of the fully-encoded key.
///
/// A single span across the family's kinds would be WRONG: the kind byte
/// sorts before the key bytes (`kind | len | key`), so e.g. the zset span
/// `[09|1|"z", 0B|1|"z"+1)` contains `09|2|"zz"` -- another key's meta.
/// Per-kind ranges only ever cover `kind | len | key ++ suffix` and are
/// therefore key-confined. An empty `upper` means "to the end of the
/// keyspace" (see `crate::store::ops::delete_range`).
pub fn family_delete_ranges(
    prefix: &[u8],
    family: CodecFamily,
    key: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    (family.0..=family.1)
        .map(|kind| {
            let lower = data_key(prefix, kind, key);
            let upper = key_upper_bound(&lower).unwrap_or_default();
            (lower, upper)
        })
        .collect()
}

/// Envelope a typed value: LEB128 varuint expire (0 = none) ++ payload.
pub fn encode_envelope(expire_ms: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + payload.len());
    let mut v = expire_ms;
    while v >= 0x80 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out.extend_from_slice(payload);
    out
}

/// Split an enveloped value -> `(expire_ms, payload)`. Overlong varints
/// (> 9 bytes) saturate at `u64::MAX` instead of failing; writers we
/// control never produce them.
pub fn decode_envelope(value: &[u8]) -> (u64, &[u8]) {
    let (mut expire, mut shift) = (0u64, 0u32);
    for (i, &b) in value.iter().enumerate() {
        if i >= 9 {
            // 10th byte: only its low bit fits in u64 -- saturate.
            return (u64::MAX, &value[10.min(value.len())..]);
        }
        if shift < 64 {
            expire |= u64::from(b & 0x7f) << shift;
        }
        shift += 7;
        if b & 0x80 == 0 {
            return (expire, &value[i + 1..]);
        }
    }
    (0, &[])
}

/// LEB128 varint count for family meta payloads (hash fields, set
/// members): meta value = `envelope ++ encode_count(n)`; the inverse
/// saturates instead of failing (writers here never produce overlong
/// varints, and a corrupt count must not break reads).
pub fn encode_count(count: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    let mut v = count;
    while v >= 0x80 {
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
    out
}

/// Decode a [`encode_count`] payload; overlong varints saturate at u64::MAX.
pub fn decode_count(payload: &[u8]) -> u64 {
    let (mut v, mut shift) = (0u64, 0u32);
    for &b in payload {
        if shift < 64 {
            v |= u64::from(b & 0x7f) << shift;
        }
        shift += 7;
        if b & 0x80 == 0 {
            return v;
        }
    }
    v
}

/// How a physical key (after the slot prefix) reads during iteration.
///
/// Rule: bytes `<= 0x18` or `== 0xFD` are typed records (kind header);
/// anything else is a raw string whose user key is the whole remainder.
///
/// COLLISION CAVEAT (accepted breaking change, documented for COMPAT.md):
/// a legacy raw string whose first byte is `<= 0x12` (e.g. a control byte)
/// is misread as a typed record. Raw strings written after this change
/// simply start with an ordinary byte in practice; 0xFD is included so
/// expire-index entries classify as typed and can be skipped by scanners.
#[derive(Debug, PartialEq, Eq)]
pub enum Classification {
    Raw,
    Typed(u8),
}

pub fn classify(after_prefix: &[u8]) -> Classification {
    match after_prefix.first() {
        Some(&b) if b <= KIND_ANN_POSTING || b == KIND_EXPIRE_INDEX => Classification::Typed(b),
        _ => Classification::Raw,
    }
}
