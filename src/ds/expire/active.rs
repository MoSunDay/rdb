//! Active expiration: the rotating index sampler and its loop.
//!
//! [`sample_once`] scans the expire index from a rotating cursor,
//! re-reads each victim to confirm it is still expired (guards against
//! racing writers), then range-deletes the family and the index entry;
//! [`spawn_active_expire`] runs the loop every 100ms with Redis-style
//! adaptive extra rounds, each round executing on tokio's blocking pool.

use std::sync::Arc;
use std::time::Duration;

use rocksdb::WriteBatch;

use crate::ds::codec;
use crate::state;
use crate::store::ops;
use crate::store::Store;

use super::{family_delete_entries, is_expired, now_ms, slot_prefix_len};

/// Upper bound on index/data keys examined per sample; keeps a pathological
/// keyspace from pinning the loop. `budget` still caps real deletions.
pub(super) const SCAN_LIMIT: usize = 1000;

/// One active-expiration round: scan index entries with
/// `expire_ms <= now_ms`, confirm-and-purge each, at most `budget`
/// deletions, resuming the scan strictly after `from` (empty = head).
/// Returns the number purged (stale index entries whose record vanished
/// or changed count too -- they needed deleting either way) plus the
/// scan cursor: the LAST KEY THE ROUND PROCESSED when `budget`/SCAN_LIMIT
/// cut the round short, or EMPTY when the scan ran to the tail -- feed
/// that back into the next call so the sampler keeps ROTATING instead
/// of always restarting at the head (high slots would starve otherwise).
/// The unprocessed stop key deliberately stays BEYOND the cursor: the
/// resume is strictly-after, so a cursor ON it would skip it entirely
/// next round.
pub fn sample_once(
    store: &Store,
    now: u64,
    budget: usize,
    from: &[u8],
    lite: Option<&crate::lite::Runtime>,
) -> (usize, Vec<u8>) {
    let mut purged = 0usize;
    let mut scanned = 0usize;
    let mut cursor = from.to_vec();
    let mut stopped = false;
    let _ = ops::for_each_from(store, from, true, &mut |k, _| {
        scanned += 1;
        if purged >= budget || scanned >= SCAN_LIMIT {
            stopped = true;
            // This key was never processed: leave the cursor on the last
            // key that WAS so the next round re-examines it.
            return false;
        }
        process_scan_key(store, k, now, &mut purged, lite);
        cursor = k.to_vec();
        true
    });
    if stopped {
        (purged, cursor)
    } else {
        (purged, Vec::new()) // hit the tail: the next round wraps to the head
    }
}

/// Handle one examined scan key for [`sample_once`]: a due index entry
/// is confirm-and-purged (and counted); data records, non-index kinds,
/// undecodable entries and not-yet-due deadlines are merely passed over.
fn process_scan_key(
    store: &Store,
    k: &[u8],
    now: u64,
    purged: &mut usize,
    lite: Option<&crate::lite::Runtime>,
) {
    let Some(plen) = slot_prefix_len(k) else {
        return;
    };
    if k.get(plen) != Some(&codec::KIND_EXPIRE_INDEX) {
        return;
    }
    let Some((expire, body)) = codec::decode_expire_index_key(k, plen) else {
        return;
    };
    if !is_expired(expire, now) {
        return; // not due; another slot may still hold due entries
    }
    if purge_indexed(store, &k[..plen], &body, expire, now, lite) {
        *purged += 1;
    }
}

/// Re-read the indexed record, then purge it (record + index) if it is
/// still expired, or drop just the stale index entry if the record
/// vanished or changed its TTL. Stays sync: it only ever runs inside
/// [`sample_once`], which the active-expire loop executes on tokio's
/// blocking pool, so the synced `batch_write` never lands on a worker.
fn purge_indexed(
    store: &Store,
    prefix: &[u8],
    body: &[u8],
    expire: u64,
    now: u64,
    lite: Option<&crate::lite::Runtime>,
) -> bool {
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
                // A reaped Lite stream family must also drop its cached
                // group offsets and queue the latched orphan sweep, or
                // the 200ms offset flusher writes orphan group records
                // onto the deleted family (lite::Runtime::stream_reaped).
                if family == codec::STREAM_FAMILY {
                    if let Some(rt) = lite {
                        rt.stream_reaped(prefix, &key);
                    }
                }
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
                // A round is sync RocksDB iteration plus synced writes:
                // park it on the blocking pool, never a tokio worker.
                let store = Arc::clone(&shared.store);
                let lite = Arc::clone(&shared.lite);
                let from = cursor.clone();
                let Ok((purged, next)) = tokio::task::spawn_blocking(move || {
                    sample_once(&store, now_ms(), BUDGET, &from, Some(&lite))
                })
                .await
                else {
                    break; // JoinError: give up this tick's extra rounds
                };
                cursor = next;
                if purged < BUDGET || rounds >= MAX_ROUNDS {
                    break;
                }
            }
            // Reaped Lite streams queued their offset-cache invalidation
            // from the blocking round; run the latched orphan sweeps now
            // (the lite background loop drains again every 200ms).
            crate::lite::drain_reaps(&shared).await;
        }
    });
}
