//! Lazy TTL path: read-time purge of expired records.
//!
//! Reads decode the envelope and purge in place when due -- Arc-holding
//! call sites commit the purge as a detached off-worker write
//! ([`ops::spawn_revalidated_write`]) guarded by a revalidation read, so
//! no fsync lands on the reading tokio worker and a racing writer that
//! replaced the record cancels the stale purge; bare-`&Store` ds-layer
//! helpers keep the inline sync.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::ds::codec::{self, CodecFamily, KIND_STRING_TTL};
use crate::store::ops;
use crate::store::Store;

use super::{family_delete_entries, is_expired, now_ms};

/// Lazy expiration: read the family root record of `key`, and if its
/// envelope says the key is due, wipe the whole family plus its index
/// entry. Returns whether a purge happened.
///
/// Sync variant for callers that only hold a bare `&Store` (ds-layer
/// resolve helpers): the delete commits with an INLINE synced write.
pub fn purge_if_expired(
    store: &Store,
    prefix: &[u8],
    family: CodecFamily,
    key: &[u8],
    now: u64,
) -> bool {
    match lazy_purge_batch(store, prefix, family, key, now) {
        Some((_, batch)) => ops::batch_write(store, batch).is_ok(),
        None => false,
    }
}

/// Lazy expiration for holders of the shared `Arc<Store>` (command-layer
/// read paths running on tokio workers): the physical delete is committed
/// by a DETACHED [`ops::spawn_revalidated_write`], so the caller never
/// fsyncs inline. The detached commit first re-reads the root and only
/// lands when the envelope still carries the SAME expire the decision
/// was based on -- a racing latched writer that rewrote the key (TTL
/// extension, value overwrite) cancels the stale purge instead of losing
/// its records to the content-agnostic family range deletes. Returns the
/// DECISION -- the envelope was due and the key is treated as gone --
/// NOT the write outcome; the RocksDB commit happens off-worker and is
/// only logged if it fails.
pub fn purge_if_expired_arc(
    store: &Arc<Store>,
    prefix: &[u8],
    family: CodecFamily,
    key: &[u8],
    now: u64,
) -> bool {
    let root = codec::data_key(prefix, family.0, key);
    match lazy_purge_batch(store, prefix, family, key, now) {
        Some((expire, batch)) => {
            ops::spawn_revalidated_write(Arc::clone(store), root, revalidate_expire(expire), batch);
            true
        }
        None => false,
    }
}

/// Revalidation probe for a detached purge: true only when the current
/// root value still decodes to the SAME expire deadline the decision
/// read -- unchanged means still due (time only moves forward), while a
/// missing or rewritten record cancels the purge.
pub(super) fn revalidate_expire(expected: u64) -> impl Fn(Option<&[u8]>) -> bool {
    move |current| match current {
        Some(val) => codec::decode_envelope(val).0 == expected,
        None => false,
    }
}

/// Decision core shared by both lazy-purge entry points: read the family
/// root, decode the envelope and, when it is due, build the batch wiping
/// the whole family plus its index entry. `None` = nothing to purge;
/// `Some((expire_ms, batch))` also carries the deadline the decision
/// was based on, so detached callers can revalidate against it.
fn lazy_purge_batch(
    store: &Store,
    prefix: &[u8],
    family: CodecFamily,
    key: &[u8],
    now: u64,
) -> Option<(u64, WriteBatch)> {
    let root = codec::data_key(prefix, family.0, key);
    let val = match ops::get_physical(store, &root) {
        Ok(Some(v)) => v,
        _ => return None,
    };
    let (expire, _) = codec::decode_envelope(&val);
    if !is_expired(expire, now) {
        return None;
    }
    let mut batch = WriteBatch::default();
    family_delete_entries(&mut batch, prefix, family, key, expire);
    Some((expire, batch))
}

/// Read a STRING_TTL record -> `(expire_ms, payload)`; `Ok(None)` when the
/// key is missing or was just lazely purged. Takes the shared
/// `Arc<Store>` so an expired record's purge is a DETACHED write (no
/// inline fsync on the reading worker).
pub fn read_enveloped(
    store: &Arc<Store>,
    prefix: &[u8],
    key: &[u8],
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let root = codec::data_key(prefix, KIND_STRING_TTL, key);
    let Some(val) = ops::get_physical(store, &root)? else {
        return Ok(None);
    };
    let (expire, payload) = codec::decode_envelope(&val);
    if is_expired(expire, now_ms()) {
        purge_if_expired_arc(store, prefix, codec::STRING_FAMILY, key, now_ms());
        return Ok(None);
    }
    Ok(Some((expire, payload.to_vec())))
}
