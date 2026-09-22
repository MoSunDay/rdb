//! Uniform TTL: envelope helpers, lazy purge and the active-expiration
//! sampler.
//!
//! Every record kind except raw strings stores
//! `<varuint expire_ms> ++ <payload>` (see `ds::codec`), so TTL is one code
//! path for all types. Keys with `expire_ms > 0` ALSO write an index record
//! `<slot_prefix> ++ 0xFD ++ <expire_ms:u64 BE> ++ <data key body>`, which
//! the sampler scans. NOTE: index entries sort slot-major (the decimal
//! slot prefix precedes 0xFD), so the sampler walks one ordered window per
//! slot rather than one global window -- accepted, documented.
//!
//! Lazy path: reads decode the envelope and purge in place when due --
//! Arc-holding call sites commit the purge as a detached off-worker
//! write ([`ops::spawn_revalidated_write`]) guarded by a revalidation
//! read, so no fsync lands on the reading tokio worker and a racing
//! writer that replaced the record cancels the stale purge;
//! bare-`&Store` ds-layer helpers keep the inline sync.
//! Active path: [`sample_once`] scans the index from a rotating cursor,
//! re-reads each victim to confirm it is still expired (guards against
//! racing writers), then range-deletes the family and the index entry.
//! [`spawn_active_expire`] runs the loop every 100ms with Redis-style
//! adaptive extra rounds, each round executing on tokio's blocking pool.

mod active;
mod lazy;

pub use active::{sample_once, spawn_active_expire};
pub use lazy::{purge_if_expired, purge_if_expired_arc, read_enveloped};

use std::time::{SystemTime, UNIX_EPOCH};

use rocksdb::WriteBatch;

use crate::ds::codec::{self, CodecFamily};

/// `expire_ms == 0` means "no expiry" -> never expired.
pub fn is_expired(expire_ms: u64, now_ms: u64) -> bool {
    expire_ms != 0 && expire_ms <= now_ms
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Maintain the expire index inside an existing write batch: drop the old
/// index entry when the value moved, add the new one when > 0. `data_key`
/// is the FULL physical root key of the record (prefix included).
pub fn set_ttl_entries(
    batch: &mut WriteBatch,
    prefix: &[u8],
    data_key: Vec<u8>,
    old_expire: u64,
    new_expire: u64,
) {
    if old_expire != new_expire && old_expire > 0 {
        batch.delete(codec::expire_index_key(prefix, old_expire, &data_key));
    }
    if new_expire > 0 {
        let idx = codec::expire_index_key(prefix, new_expire, &data_key);
        batch.put(idx, b"");
    }
}

/// Batch entries that fully remove one key's family and its index entry.
pub fn family_delete_entries(
    batch: &mut WriteBatch,
    prefix: &[u8],
    family: CodecFamily,
    key: &[u8],
    expire: u64,
) {
    // Per-kind ranges: a single family-wide span would swallow other
    // keys' records (kind byte sorts before the key bytes).
    for (lower, upper) in codec::family_delete_ranges(prefix, family, key) {
        batch.delete_range(lower, upper);
    }
    // Stream deletes also drop the Kafka committed-offset ledger rows
    // (KIND_STREAM_OFFSET sits in its own family far above the stream
    // span -- a single 0x0C..=0x20 range would swallow JSON/vectorset/
    // search records -- so the window is folded in explicitly here).
    if family == codec::STREAM_FAMILY {
        for (lower, upper) in codec::family_delete_ranges(prefix, codec::OFFSET_FAMILY, key) {
            batch.delete_range(lower, upper);
        }
    }
    if expire > 0 {
        let root = codec::data_key(prefix, family.0, key);
        batch.delete(codec::expire_index_key(prefix, expire, &root));
    }
}

/// Length of a leading `"<decimal slot>/"` prefix, if `k` starts with one.
pub fn slot_prefix_len(k: &[u8]) -> Option<usize> {
    let digits = k.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 5 || k.get(digits) != Some(&b'/') {
        return None;
    }
    Some(digits + 1)
}

#[cfg(test)]
mod tests;
