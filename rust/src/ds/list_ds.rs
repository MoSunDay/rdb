//! List storage ops: a two-ended deque encoded as two grow-only index
//! ranges. Physical layout, one user key per list:
//!
//! ```text
//! meta  = data_key(prefix, KIND_LIST_META, key)
//!         value = envelope ++ LEB128(l_count) ++ LEB128(l_next)
//!                                  ++ LEB128(r_count) ++ LEB128(r_next)
//! left  = elem_key(prefix, KIND_LIST_L, key, (u64::MAX - l).to_be_bytes())
//!         value = raw element bytes (no envelope; TTL is per-key via meta)
//! right = elem_key(prefix, KIND_LIST_R, key, r.to_be_bytes())
//!         value = raw element bytes
//! ```
//!
//! The L suffix is COMPLEMENTED (`u64::MAX - l`), so ascending physical
//! order visits descending `l`: the L side reads front-to-back (logical
//! order) in one forward scan, and so does the R side (ascending `r`).
//!
//! INVARIANT -- after every mutating command the live entries of a side
//! are exactly the index range `[base, next)` with `base = next - count`
//! (NO holes). Commands that drop interior entries (LREM/LSET/LINSERT
//! cascades, later phases) must compact their side so the range stays
//! dense. Logical order = L entries ascending-physical ++ R entries
//! ascending-physical; `len = l_count + r_count`. Pops prefer their own
//! side and only touch the far side when theirs is empty.

use rocksdb::WriteBatch;

use crate::ds::codec::{self, KIND_LIST_L, KIND_LIST_META, KIND_LIST_R, LIST_FAMILY};
use crate::ds::expire;
use crate::store::{ops, Store};

/// Fixed suffix width of every L/R entry key (u64 big-endian).
pub const SUFFIX_LEN: usize = 8;

/// Root counters of one list: both grow-only index ranges plus the TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListMeta {
    pub expire_ms: u64,
    /// Live entries on the left side: `l in [l_base, l_next)`.
    pub l_count: u64,
    /// Next free left index (one past the newest L entry).
    pub l_next: u64,
    /// Live entries on the right side: `r in [r_base, r_next)`.
    pub r_count: u64,
    /// Next free right index (one past the newest R entry).
    pub r_next: u64,
}

impl ListMeta {
    /// Total live elements across both sides.
    pub fn len(&self) -> u64 {
        self.l_count + self.r_count
    }

    /// `true` when both sides are empty (fresh/blanked meta).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Oldest live L index; only meaningful when `l_count > 0`.
    pub fn l_base(&self) -> u64 {
        self.l_next - self.l_count
    }

    /// Oldest live R index; only meaningful when `r_count > 0`.
    pub fn r_base(&self) -> u64 {
        self.r_next - self.r_count
    }
}

/// Result of reading a list's meta record.
#[derive(Debug, PartialEq, Eq)]
pub enum ListMetaRead {
    /// No meta record: the list does not exist.
    Missing,
    /// Live list: TTL plus both index ranges.
    Present(ListMeta),
    /// Expired: the whole family was just purged.
    Purged,
    /// Store error.
    Failed(String),
}

/// Left-entry key suffix: complemented big-endian so physical order is
/// descending `l` (logical front-to-back of the L side).
fn l_suffix(l: u64) -> [u8; SUFFIX_LEN] {
    (u64::MAX - l).to_be_bytes()
}

/// Right-entry key suffix: plain big-endian, ascending `r`.
fn r_suffix(r: u64) -> [u8; SUFFIX_LEN] {
    r.to_be_bytes()
}

/// Serialize the four counters (fixed order: l_count, l_next, r_count,
/// r_next) as concatenated LEB128 varints.
pub(crate) fn encode_meta_payload(meta: &ListMeta) -> Vec<u8> {
    let mut out = codec::encode_count(meta.l_count);
    out.extend(codec::encode_count(meta.l_next));
    out.extend(codec::encode_count(meta.r_count));
    out.extend(codec::encode_count(meta.r_next));
    out
}

/// Decode one LEB128 varint -> `(value, remainder)`. Truncated varints
/// saturate like `codec::decode_count`; writers never produce them.
fn next_varint(bytes: &[u8]) -> (u64, &[u8]) {
    let (mut v, mut shift) = (0u64, 0u32);
    for (i, &b) in bytes.iter().enumerate() {
        if shift < 64 {
            v |= u64::from(b & 0x7f) << shift;
        }
        shift += 7;
        if b & 0x80 == 0 {
            return (v, &bytes[i + 1..]);
        }
    }
    (v, &[])
}

/// Parse a meta payload back into counters (missing fields read as 0).
pub(crate) fn decode_meta_payload(expire_ms: u64, payload: &[u8]) -> ListMeta {
    let (l_count, rest) = next_varint(payload);
    let (l_next, rest) = next_varint(rest);
    let (r_count, rest) = next_varint(rest);
    let (r_next, _) = next_varint(rest);
    ListMeta {
        expire_ms,
        l_count,
        l_next,
        r_count,
        r_next,
    }
}

/// Read + lazily expire one list's meta. Wrong-type detection is the
/// command layer's job (via `keys_core::resolve`).
pub fn read_meta(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> ListMetaRead {
    let root = meta_key(prefix, key);
    let val = match ops::get_physical(store, &root) {
        Err(e) => return ListMetaRead::Failed(e),
        Ok(None) => return ListMetaRead::Missing,
        Ok(Some(v)) => v,
    };
    let (expire_ms, payload) = codec::decode_envelope(&val);
    if expire::is_expired(expire_ms, now) {
        return if expire::purge_if_expired(store, prefix, LIST_FAMILY, key, now) {
            ListMetaRead::Purged
        } else {
            ListMetaRead::Failed("purge failed".to_string())
        };
    }
    ListMetaRead::Present(decode_meta_payload(expire_ms, payload))
}

/// Meta/root physical key of a list.
pub fn meta_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_LIST_META, key)
}

/// Put the meta record (envelope keeps the existing TTL) into `batch`.
pub fn write_meta(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], meta: &ListMeta) {
    batch.put(
        meta_key(prefix, key),
        codec::encode_envelope(meta.expire_ms, &encode_meta_payload(meta)),
    );
}

/// Batch entries wiping the whole list family and its TTL index entry.
pub fn delete_family(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], expire_ms: u64) {
    expire::family_delete_entries(batch, prefix, LIST_FAMILY, key, expire_ms);
}

/// Physical key of the left entry at index `l`.
pub fn l_key(prefix: &[u8], key: &[u8], l: u64) -> Vec<u8> {
    codec::elem_key(prefix, KIND_LIST_L, key, &l_suffix(l))
}

/// Physical key of the right entry at index `r`.
pub fn r_key(prefix: &[u8], key: &[u8], r: u64) -> Vec<u8> {
    codec::elem_key(prefix, KIND_LIST_R, key, &r_suffix(r))
}

/// Put one left entry (raw element bytes, no envelope).
pub fn put_l(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], l: u64, elem: &[u8]) {
    batch.put(l_key(prefix, key, l), elem);
}

/// Put one right entry (raw element bytes, no envelope).
pub fn put_r(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], r: u64, elem: &[u8]) {
    batch.put(r_key(prefix, key, r), elem);
}

/// Drop one left entry.
pub fn del_l(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], l: u64) {
    batch.delete(l_key(prefix, key, l));
}

/// Drop one right entry.
pub fn del_r(batch: &mut WriteBatch, prefix: &[u8], key: &[u8], r: u64) {
    batch.delete(r_key(prefix, key, r));
}

/// One left entry; `Ok(None)` = no record at that index.
pub fn get_l(store: &Store, prefix: &[u8], key: &[u8], l: u64) -> Result<Option<Vec<u8>>, String> {
    ops::get_physical(store, &l_key(prefix, key, l))
}

/// One right entry; `Ok(None)` = no record at that index.
pub fn get_r(store: &Store, prefix: &[u8], key: &[u8], r: u64) -> Result<Option<Vec<u8>>, String> {
    ops::get_physical(store, &r_key(prefix, key, r))
}

/// Decode the trailing 8-byte big-endian index of an entry key; `None`
/// when the key is too short to carry one (defensive against strays).
fn suffix_index(k: &[u8]) -> Option<u64> {
    let start = k.len().checked_sub(SUFFIX_LEN)?;
    let bytes = k.get(start..)?;
    Some(u64::from_be_bytes(bytes.try_into().ok()?))
}

/// Bounded ascending-physical scan of one side's live index window
/// `[lo, hi]` (inclusive): starts at the window's lowest physical key
/// (`l_key(hi)` / `r_key(lo)`) and stops as soon as a key leaves the
/// key-confined kind window (prefix check -- every key inside starts
/// with `data_key(kind, key)`) or its decoded index exits `[lo, hi]`.
/// `f(index, elem)` returns `false` to stop early.
fn for_each_side(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    left: bool,
    lo: u64,
    hi: u64,
    f: &mut dyn FnMut(u64, &[u8]) -> bool,
) -> Result<(), String> {
    let from = if left {
        l_key(prefix, key, hi)
    } else {
        r_key(prefix, key, lo)
    };
    let base = codec::data_key(prefix, if left { KIND_LIST_L } else { KIND_LIST_R }, key);
    ops::for_each_from(store, &from, false, &mut |k, v| {
        if !k.starts_with(&base) {
            return false; // left this key's kind window
        }
        let Some(raw) = suffix_index(k) else {
            return false;
        };
        // L suffixes are stored complemented (u64::MAX - l); report and
        // bound-check the TRUE index on both sides.
        let idx = if left { u64::MAX - raw } else { raw };
        if (left && idx < lo) || (!left && idx > hi) {
            return false; // decoded index left the live window
        }
        f(idx, v)
    })
}

/// Logical 0-based position for a possibly-negative index (Redis rule:
/// negatives count from the back, `-1` = last). `None` when out of range.
pub fn position_of(meta: &ListMeta, pos: i64) -> Option<u64> {
    let len = i64::try_from(meta.len()).unwrap_or(i64::MAX);
    let p = if pos < 0 { len + pos } else { pos };
    if p >= 0 {
        u64::try_from(p).ok().filter(|&p| p < meta.len())
    } else {
        None
    }
}

/// Logical (already resolved, 0-based) position -> `(is_left, index)`.
/// `is_left = p < l_count`; L index counts down from the newest L entry,
/// R index counts up from `r_base`.
pub fn locate(meta: &ListMeta, p: u64) -> (bool, u64) {
    if p < meta.l_count {
        (true, meta.l_base() + meta.l_count - 1 - p)
    } else {
        (false, meta.r_base() + p - meta.l_count)
    }
}

/// Pop target from the logical LEFT: the newest L entry when the left
/// side is loaded, else the OLDEST R entry (`r_base`).
pub fn pop_left_target(meta: &ListMeta) -> (bool, u64) {
    if meta.l_count > 0 {
        (true, meta.l_next - 1)
    } else {
        (false, meta.r_base())
    }
}

/// Pop target from the logical RIGHT: the newest R entry when the right
/// side is loaded, else the OLDEST L entry (`l_base`).
pub fn pop_right_target(meta: &ListMeta) -> (bool, u64) {
    if meta.r_count > 0 {
        (false, meta.r_next - 1)
    } else {
        (true, meta.l_base())
    }
}

/// Elements at logical positions `[start..=stop]` (inclusive; empty when
/// `start > stop`, `start >= len` or the list is blank), in logical
/// order. At most two window scans: the L window (if it intersects the
/// range) then the R window; each walks ascending-physical keys with the
/// suffix-decoded index enforcing the bounds.
pub fn collect_range(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    meta: &ListMeta,
    start: u64,
    stop: u64,
) -> Result<Vec<Vec<u8>>, String> {
    let len = meta.len();
    if start >= len || start > stop {
        return Ok(Vec::new());
    }
    let stop = stop.min(len - 1);
    let mut out: Vec<Vec<u8>> = Vec::new();
    // L window: p in [start, min(stop, l_count-1)] -> l DESCENDING from
    // l_base + l_count - 1 - start down to l_base.
    if start < meta.l_count {
        let l_hi = meta.l_base() + meta.l_count - 1 - start;
        let l_lo = meta.l_base() + (meta.l_count - 1 - stop.min(meta.l_count - 1));
        for_each_side(store, prefix, key, true, l_lo, l_hi, &mut |_, elem| {
            out.push(elem.to_vec());
            true
        })?;
    }
    // R window: p in [max(start, l_count), stop] -> r ASCENDING from r_base.
    if stop >= meta.l_count {
        let p0 = start.max(meta.l_count);
        let r_lo = meta.r_base() + p0 - meta.l_count;
        let r_hi = meta.r_base() + stop - meta.l_count;
        for_each_side(store, prefix, key, false, r_lo, r_hi, &mut |_, elem| {
            out.push(elem.to_vec());
            true
        })?;
    }
    Ok(out)
}

/// All live entries of one side with their index, physical order: for L
/// that is descending `l` (= logical order), for R ascending `r`.
pub fn collect_side(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    meta: &ListMeta,
    left: bool,
) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let mut out = Vec::new();
    if left && meta.l_count == 0 || !left && meta.r_count == 0 {
        return Ok(out);
    }
    let (lo, hi) = if left {
        (meta.l_base(), meta.l_next - 1)
    } else {
        (meta.r_base(), meta.r_next - 1)
    };
    for_each_side(store, prefix, key, left, lo, hi, &mut |idx, elem| {
        out.push((idx, elem.to_vec()));
        true
    })?;
    Ok(out)
}
