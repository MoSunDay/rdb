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
//! Active path: [`sample_once`] scans the index from a rotating cursor,
//! re-reads each victim to confirm it is still expired (guards against
//! racing writers), then range-deletes the family and the index entry.
//! [`spawn_active_expire`] runs the loop every 100ms with Redis-style
//! adaptive extra rounds.

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
/// deletions, resuming the scan strictly after `from` (empty = head).
/// Returns the number purged (stale index entries whose record vanished
/// or changed count too -- they needed deleting either way) plus the
/// scan cursor: the key the round stopped on when `budget`/SCAN_LIMIT
/// cut it short, or EMPTY when the scan ran to the tail -- feed that
/// back into the next call so the sampler keeps ROTATING instead of
/// always restarting at the head (high slots would starve otherwise).
pub fn sample_once(store: &Store, now: u64, budget: usize, from: &[u8]) -> (usize, Vec<u8>) {
    let mut purged = 0usize;
    let mut scanned = 0usize;
    let mut cursor = from.to_vec();
    let mut stopped = false;
    let _ = ops::for_each_from(store, from, true, &mut |k, _| {
        cursor = k.to_vec();
        scanned += 1;
        if purged >= budget || scanned > SCAN_LIMIT {
            stopped = true;
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
    if stopped {
        (purged, cursor)
    } else {
        (purged, Vec::new()) // hit the tail: the next round wraps to the head
    }
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
/// rounds (Redis adaptive behavior) before sleeping again. The scan
/// cursor survives ticks and extra rounds alike, so consecutive rounds
/// keep advancing through the keyspace instead of re-reading the head.
pub fn spawn_active_expire(shared: Arc<state::Shared>) {
    const BUDGET: usize = 20;
    const MAX_ROUNDS: usize = 5;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            ticker.tick().await;
            let mut rounds = 0;
            loop {
                rounds += 1;
                let (purged, next) = sample_once(&shared.store, now_ms(), BUDGET, &cursor);
                cursor = next;
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
        write_enveloped_at(store, P, kind, key, expire, payload);
    }

    fn write_enveloped_at(
        store: &Store,
        prefix: &[u8],
        kind: u8,
        key: &[u8],
        expire: u64,
        payload: &[u8],
    ) {
        let root = codec::data_key(prefix, kind, key);
        let mut batch = WriteBatch::default();
        batch.put(&root, codec::encode_envelope(expire, payload));
        set_ttl_entries(&mut batch, prefix, root, 0, expire);
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

        assert_eq!(sample_once(&store, 200, 10, b"").0, 2);
        assert_eq!(read_enveloped(&store, P, b"due").unwrap(), None);
        // element went with the meta
        assert_eq!(ops::get_physical(&store, &elem).unwrap(), None);
        // not-yet-due untouched
        let (expire, payload) = read_enveloped(&store, P, b"later").unwrap().unwrap();
        assert_eq!((expire, payload), (9_000_000_000_000, b"v".to_vec()));
        // index entries for the purged keys are gone; a resample is idle
        assert_eq!(sample_once(&store, 200, 10, b"").0, 0);
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
        assert_eq!(sample_once(&store, 500, 10, b"").0, 0);
        let (expire, payload) = read_enveloped(&store, P, b"k").unwrap().unwrap();
        assert_eq!((expire, payload), (0, b"v".to_vec()));
    }

    #[test]
    fn lazy_purge_deletes_family_and_index() {
        let (_dir, store) = open_tmp("lazy");
        write_enveloped(&store, KIND_STRING_TTL, b"k", 5, b"v");
        assert!(purge_if_expired(&store, P, STRING_FAMILY, b"k", 10));
        assert!(!purge_if_expired(&store, P, STRING_FAMILY, b"k", 10));
        assert_eq!(sample_once(&store, 10, 10, b"").0, 0);
        // an index entry whose record vanished is still swept
        let mut batch = WriteBatch::default();
        batch.put(
            codec::expire_index_key(P, 5, &codec::data_key(P, KIND_STRING_TTL, b"ghost")),
            b"",
        );
        ops::batch_write(&store, batch).unwrap();
        assert_eq!(sample_once(&store, 10, 10, b"").0, 1);
    }

    #[test]
    fn sampler_cursor_resumes_after_stop() {
        let (_dir, store) = open_tmp("resume");
        write_enveloped_at(&store, b"10/", KIND_STRING_TTL, b"k", 100, b"v");
        write_enveloped_at(&store, b"99/", KIND_STRING_TTL, b"k", 100, b"v");
        // budget=1: the round purges the 10/ victim, then stops on the
        // next key it touches -- the 99/ data record.
        let (purged, cursor) = sample_once(&store, 200, 1, b"");
        assert_eq!(purged, 1);
        assert_eq!(
            cursor,
            codec::data_key(b"99/", KIND_STRING_TTL, b"k"),
            "cursor sits on the last key the round accessed"
        );
        // resuming after that cursor clears the 99/ victim in slot order
        let (purged2, cursor2) = sample_once(&store, 200, 1, &cursor);
        assert_eq!(purged2, 1);
        assert!(cursor2.is_empty(), "scan reached the tail and wrapped");
        assert_eq!(read_enveloped(&store, b"10/", b"k").unwrap(), None);
        assert_eq!(read_enveloped(&store, b"99/", b"k").unwrap(), None);
    }

    #[test]
    fn sampler_cursor_wraps_after_tail() {
        let (_dir, store) = open_tmp("wrap");
        write_enveloped_at(&store, b"99/", KIND_STRING_TTL, b"tail", 100, b"v");
        let (purged, cursor) = sample_once(&store, 200, 10, b"");
        assert_eq!(purged, 1);
        assert!(
            cursor.is_empty(),
            "natural exhaustion returns an empty cursor"
        );
        // a NEW victim sorting before everything the sweep just saw: only
        // a wrapped (head restart) round can reach it
        write_enveloped_at(&store, b"10/", KIND_STRING_TTL, b"head", 100, b"v");
        let (purged2, cursor2) = sample_once(&store, 200, 10, &cursor);
        assert_eq!(purged2, 1, "wrapped round restarts from the head");
        assert!(cursor2.is_empty());
        assert_eq!(read_enveloped(&store, b"10/", b"head").unwrap(), None);
        assert_eq!(read_enveloped(&store, b"99/", b"tail").unwrap(), None);
    }

    #[test]
    fn sampler_reaches_keys_past_scan_limit() {
        let (_dir, store) = open_tmp("limit");
        // >SCAN_LIMIT live records at slot 10/ push the cursor forward one
        // SCAN_LIMIT-window per round (written in one batch to keep the
        // test off the fsync path).
        let far = 9_000_000_000_000u64;
        let mut batch = WriteBatch::default();
        for i in 0..=SCAN_LIMIT {
            let key = format!("bulk{i:04}").into_bytes();
            let root = codec::data_key(b"10/", KIND_STRING_TTL, &key);
            batch.put(&root, codec::encode_envelope(far, b"v"));
            batch.put(codec::expire_index_key(b"10/", far, &root), b"");
        }
        ops::batch_write(&store, batch).unwrap();
        // the due key sits in a slot BEYOND the bulk window
        write_enveloped_at(&store, b"99/", KIND_STRING_TTL, b"due", 100, b"v");
        let due = codec::data_key(b"99/", KIND_STRING_TTL, b"due");

        let mut cursor = Vec::new();
        let mut rounds = 0;
        while ops::get_physical(&store, &due).unwrap().is_some() {
            rounds += 1;
            assert!(rounds < 16, "cursor rotation never reached the high slot");
            let (_, next) = sample_once(&store, 200, 20, &cursor);
            cursor = next;
        }
        assert!(rounds > 1, "the scan limit forced multiple rounds");
        // the live bulk records were only ever passed over, never purged
        assert_eq!(
            read_enveloped(&store, b"10/", b"bulk0000")
                .unwrap()
                .unwrap()
                .0,
            far
        );
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
