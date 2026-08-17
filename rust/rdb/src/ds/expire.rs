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
//! Lazy path: reads decode the envelope and purge in place when due.
//! Active path: [`sample_once`] scans the index, re-reads each victim to
//! confirm it is still expired (guards against racing writers), then
//! range-deletes the family and the index entry. [`spawn_active_expire`]
//! runs the loop every 100ms with Redis-style adaptive extra rounds.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rocksdb::WriteBatch;

use crate::ds::codec::{self, CodecFamily, KIND_STRING_TTL};
use crate::state;
use crate::store::ops;
use crate::store::Store;

/// Upper bound on index/data keys examined per sample; keeps a pathological
/// keyspace from pinning the loop. `budget` still caps real deletions.
const SCAN_LIMIT: usize = 1000;

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

/// Lazy expiration: read the family root record of `key`, and if its
/// envelope says the key is due, wipe the whole family plus its index
/// entry. Returns whether a purge happened.
pub fn purge_if_expired(
    store: &Store,
    prefix: &[u8],
    family: CodecFamily,
    key: &[u8],
    now: u64,
) -> bool {
    let root = codec::data_key(prefix, family.0, key);
    let val = match ops::get_physical(store, &root) {
        Ok(Some(v)) => v,
        _ => return false,
    };
    let (expire, _) = codec::decode_envelope(&val);
    if !is_expired(expire, now) {
        return false;
    }
    let mut batch = WriteBatch::default();
    family_delete_entries(&mut batch, prefix, family, key, expire);
    ops::batch_write(store, batch).is_ok()
}

/// Read a STRING_TTL record -> `(expire_ms, payload)`; `Ok(None)` when the
/// key is missing or was just lazely purged.
pub fn read_enveloped(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let root = codec::data_key(prefix, KIND_STRING_TTL, key);
    let Some(val) = ops::get_physical(store, &root)? else {
        return Ok(None);
    };
    let (expire, payload) = codec::decode_envelope(&val);
    if is_expired(expire, now_ms()) {
        purge_if_expired(store, prefix, codec::STRING_FAMILY, key, now_ms());
        return Ok(None);
    }
    Ok(Some((expire, payload.to_vec())))
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

/// One active-expiration round: scan index entries with
/// `expire_ms <= now_ms`, confirm-and-purge each, at most `budget`
/// deletions. Returns the number purged (stale index entries whose record
/// vanished or changed count too -- they needed deleting either way).
pub fn sample_once(store: &Store, now: u64, budget: usize) -> usize {
    let mut purged = 0usize;
    let mut scanned = 0usize;
    let _ = ops::for_each_from(store, b"", false, &mut |k, _| {
        scanned += 1;
        if purged >= budget || scanned > SCAN_LIMIT {
            return false;
        }
        let Some(plen) = slot_prefix_len(k) else {
            return true;
        };
        if k.get(plen) != Some(&codec::KIND_EXPIRE_INDEX) {
            return true;
        }
        let Some((expire, body)) = codec::decode_expire_index_key(k, plen) else {
            return true;
        };
        if !is_expired(expire, now) {
            return true; // not due; another slot may still hold due entries
        }
        if purge_indexed(store, &k[..plen], &body, expire, now) {
            purged += 1;
        }
        true
    });
    purged
}

/// Re-read the indexed record, then purge it (record + index) if it is
/// still expired, or drop just the stale index entry if the record
/// vanished or changed its TTL.
fn purge_indexed(store: &Store, prefix: &[u8], body: &[u8], expire: u64, now: u64) -> bool {
    let index_key = {
        let mut k = prefix.to_vec();
        k.push(codec::KIND_EXPIRE_INDEX);
        k.extend_from_slice(&expire.to_be_bytes());
        k.extend_from_slice(body);
        k
    };
    let Some(kind) = body.first().copied() else {
        return false;
    };
    let Some(family) = codec::family_of(kind) else {
        return false; // raw strings never carry index entries
    };
    let mut data_key = prefix.to_vec();
    data_key.extend_from_slice(body);
    match ops::get_physical(store, &data_key) {
        Err(_) => false,
        Ok(None) => {
            // record already gone: stale index entry only
            let mut batch = WriteBatch::default();
            batch.delete(index_key);
            ops::batch_write(store, batch).is_ok()
        }
        Ok(Some(val)) => {
            let (current, _) = codec::decode_envelope(&val);
            let mut batch = WriteBatch::default();
            if current == expire && is_expired(current, now) {
                let Some((_, key, _)) = codec::decode_data_key(&data_key, prefix.len()) else {
                    return false;
                };
                family_delete_entries(&mut batch, prefix, family, &key, expire);
            } else {
                batch.delete(index_key); // TTL moved or cleared: stale entry
            }
            ops::batch_write(store, batch).is_ok()
        }
    }
}

/// Active-expiration loop: every 100ms sample `budget` deletions; when the
/// whole budget was due (busy keyspace), take up to 4 extra immediate
/// rounds (Redis adaptive behavior) before sleeping again.
pub fn spawn_active_expire(shared: Arc<state::Shared>) {
    const BUDGET: usize = 20;
    const MAX_ROUNDS: usize = 5;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let mut rounds = 0;
            loop {
                rounds += 1;
                let purged = sample_once(&shared.store, now_ms(), BUDGET);
                if purged < BUDGET || rounds >= MAX_ROUNDS {
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ds::codec::{KIND_HASH_META, STRING_FAMILY};
    use crate::store::rocksdb;

    fn open_tmp(_tag: &str) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = rocksdb::open(dir.path().to_str().unwrap()).expect("open");
        (dir, store)
    }

    const P: &[u8] = b"70/";

    fn write_enveloped(store: &Store, kind: u8, key: &[u8], expire: u64, payload: &[u8]) {
        let root = codec::data_key(P, kind, key);
        let mut batch = WriteBatch::default();
        batch.put(&root, codec::encode_envelope(expire, payload));
        set_ttl_entries(&mut batch, P, root, 0, expire);
        ops::batch_write(store, batch).unwrap();
    }

    #[test]
    fn is_expired_zero_never() {
        assert!(!is_expired(0, u64::MAX));
        assert!(!is_expired(10, 9));
        assert!(is_expired(10, 10));
        assert!(is_expired(10, 11));
    }

    #[test]
    fn read_enveloped_missing_and_live() {
        let (_dir, store) = open_tmp("read");
        assert_eq!(read_enveloped(&store, P, b"k").unwrap(), None);
        write_enveloped(&store, KIND_STRING_TTL, b"k", 9_000_000_000_000, b"val");
        let (expire, payload) = read_enveloped(&store, P, b"k").unwrap().unwrap();
        assert_eq!(expire, 9_000_000_000_000);
        assert_eq!(payload, b"val".to_vec());
    }

    #[test]
    fn sampler_purges_due_entries_only() {
        let (_dir, store) = open_tmp("sample");
        write_enveloped(&store, KIND_STRING_TTL, b"due", 100, b"v");
        write_enveloped(&store, KIND_STRING_TTL, b"later", 9_000_000_000_000, b"v");
        write_enveloped(&store, KIND_HASH_META, b"h", 50, b"meta");
        // element record under the hash family: must vanish with the meta
        rocksdb::set(&store, P, b"", b"").ok();
        let elem = codec::elem_key(P, crate::ds::codec::KIND_HASH_FLD, b"h", b"f");
        let mut batch = WriteBatch::default();
        batch.put(&elem, b"x");
        ops::batch_write(&store, batch).unwrap();

        assert_eq!(sample_once(&store, 200, 10), 2);
        assert_eq!(read_enveloped(&store, P, b"due").unwrap(), None);
        // element went with the meta
        assert_eq!(ops::get_physical(&store, &elem).unwrap(), None);
        // not-yet-due untouched
        let (expire, payload) = read_enveloped(&store, P, b"later").unwrap().unwrap();
        assert_eq!((expire, payload), (9_000_000_000_000, b"v".to_vec()));
        // index entries for the purged keys are gone; a resample is idle
        assert_eq!(sample_once(&store, 200, 10), 0);
    }

    #[test]
    fn ttl_removed_cleans_index() {
        let (_dir, store) = open_tmp("ttlrm");
        write_enveloped(&store, KIND_STRING_TTL, b"k", 111, b"v");
        let root = codec::data_key(P, KIND_STRING_TTL, b"k");
        let mut batch = WriteBatch::default();
        batch.put(&root, codec::encode_envelope(0, b"v"));
        set_ttl_entries(&mut batch, P, root, 111, 0);
        ops::batch_write(&store, batch).unwrap();
        assert_eq!(sample_once(&store, 500, 10), 0);
        let (expire, payload) = read_enveloped(&store, P, b"k").unwrap().unwrap();
        assert_eq!((expire, payload), (0, b"v".to_vec()));
    }

    #[test]
    fn lazy_purge_deletes_family_and_index() {
        let (_dir, store) = open_tmp("lazy");
        write_enveloped(&store, KIND_STRING_TTL, b"k", 5, b"v");
        assert!(purge_if_expired(&store, P, STRING_FAMILY, b"k", 10));
        assert!(!purge_if_expired(&store, P, STRING_FAMILY, b"k", 10));
        assert_eq!(sample_once(&store, 10, 10), 0);
        // an index entry whose record vanished is still swept
        let mut batch = WriteBatch::default();
        batch.put(
            codec::expire_index_key(P, 5, &codec::data_key(P, KIND_STRING_TTL, b"ghost")),
            b"",
        );
        ops::batch_write(&store, batch).unwrap();
        assert_eq!(sample_once(&store, 10, 10), 1);
    }

    #[test]
    fn slot_prefix_len_parses() {
        assert_eq!(slot_prefix_len(b"70/\x02"), Some(3));
        assert_eq!(slot_prefix_len(b"0/x"), Some(2));
        assert_eq!(slot_prefix_len(b"16383/y"), Some(6));
        assert_eq!(slot_prefix_len(b"noblash"), None);
        assert_eq!(slot_prefix_len(b"123456/"), None); // 6 digits: not a slot
        assert_eq!(slot_prefix_len(b""), None);
        assert_eq!(slot_prefix_len(b"70"), None);
    }
}
