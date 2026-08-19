//! Consumer-group offset cache: in-memory delivered/committed watermarks
//! with periodic (200ms) disk snapshots for crash resume.
//!
//! Lite semantics (no PEL): `delivered` advances on every XREADGROUP `>`
//! delivery but is memory-only between flushes; `committed` advances on
//! XACK and is what survives a crash. On lazy load after a restart the
//! effective delivery point is clamped to `committed`, so un-acked
//! messages are redelivered -- at-least-once, matching RocketMQ Lite.
//!
//! Flushing swaps the dirty set out under one lock write-guard and builds
//! the RocksDB batch OUTSIDE the lock: acks racing the swap simply stay
//! dirty and ride the next round.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use rocksdb::WriteBatch;

use super::model::{self, EntryId, GroupPayload};

/// Cached state of one (stream, group).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupState {
    pub created_ms: u64,
    pub delivered: EntryId,
    pub committed: EntryId,
}

/// Cache keys are RAW BYTES `(stream, group)`, never lossy-decoded
/// Strings: group names are not charset-validated at the command layer,
/// and `from_utf8_lossy` would collapse every invalid-UTF8 name to
/// U+FFFD -- distinct groups would share one cache entry (phantom
/// NOGROUP hits, cross-group acks). The persisted records were already
/// byte-keyed (`model::group_key`); only this cache was lossy.
struct Inner {
    map: HashMap<(Vec<u8>, Vec<u8>), GroupState>,
    dirty: HashSet<(Vec<u8>, Vec<u8>)>,
}

pub struct OffsetCache {
    inner: RwLock<Inner>,
}

pub fn new_cache() -> OffsetCache {
    OffsetCache {
        inner: RwLock::new(Inner {
            map: HashMap::new(),
            dirty: HashSet::new(),
        }),
    }
}

/// Lazily load a group into the cache. `None` when the group record does
/// not exist (misses are not cached, so a later XGROUP CREATE works).
/// Restart rule: delivery resumes from `committed`.
pub fn load(
    cache: &OffsetCache,
    store: &crate::store::Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> Result<Option<GroupState>, String> {
    let key = (stream.to_vec(), group.to_vec());
    {
        let read = cache.inner.read().unwrap();
        if let Some(st) = read.map.get(&key) {
            return Ok(Some(*st));
        }
    }
    let loaded = model::read_group(store, prefix, stream, group)?;
    let mut write = cache.inner.write().unwrap();
    // Double-check: a concurrent loader may have won the race.
    if let Some(st) = write.map.get(&key) {
        return Ok(Some(*st));
    }
    let Some(p) = loaded else { return Ok(None) };
    let st = GroupState {
        created_ms: p.created_ms,
        delivered: EntryId {
            ms: p.committed_ms,
            seq: p.committed_seq,
        },
        committed: EntryId {
            ms: p.committed_ms,
            seq: p.committed_seq,
        },
    };
    write.map.insert(key, st);
    Ok(Some(st))
}

/// XGROUP CREATE path: insert a fresh state (clean -- CREATE persists it).
pub fn insert_new(cache: &OffsetCache, stream: &[u8], group: &[u8], st: GroupState) {
    cache
        .inner
        .write()
        .unwrap()
        .map
        .insert((stream.to_vec(), group.to_vec()), st);
}

/// XREADGROUP `>`: advance the memory-only delivery watermark.
pub fn advance_delivered(cache: &OffsetCache, stream: &[u8], group: &[u8], id: EntryId) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_vec(), group.to_vec());
    if let Some(st) = write.map.get_mut(&key) {
        if id > st.delivered {
            st.delivered = id;
        }
    }
}

/// XACK: `committed = max(committed, max id)`; returns how many of `ids`
/// were beyond the old watermark (the "newly acked" count).
pub fn ack(cache: &OffsetCache, stream: &[u8], group: &[u8], ids: &[EntryId]) -> Option<usize> {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_vec(), group.to_vec());
    let st = write.map.get_mut(&key)?;
    let old = st.committed;
    let mut count = 0usize;
    for id in ids {
        if *id > st.committed {
            st.committed = *id;
        }
        if *id > old {
            count += 1;
        }
    }
    if st.committed > old {
        if st.committed > st.delivered {
            st.delivered = st.committed;
        }
        write.dirty.insert(key);
    }
    Some(count)
}

/// XGROUP SETID: reset the whole resume position (operator action),
/// persisted with the next flush round.
pub fn set_position(cache: &OffsetCache, stream: &[u8], group: &[u8], id: EntryId) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_vec(), group.to_vec());
    if let Some(st) = write.map.get_mut(&key) {
        st.delivered = id;
        st.committed = id;
        write.dirty.insert(key);
    }
}

pub fn remove_group(cache: &OffsetCache, stream: &[u8], group: &[u8]) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_vec(), group.to_vec());
    write.map.remove(&key);
    write.dirty.remove(&key);
}

/// Drop every cached group of `stream` (XGROUP-less re-create of a stream).
pub fn remove_stream(cache: &OffsetCache, stream: &[u8]) {
    let mut write = cache.inner.write().unwrap();
    write.map.retain(|(s, _), _| s != stream);
    write.dirty.retain(|(s, _)| s != stream);
}

/// Snapshot + clear the dirty set (batch construction happens off-lock).
pub fn flush_dirty(cache: &OffsetCache) -> Vec<((Vec<u8>, Vec<u8>), GroupState)> {
    let mut write = cache.inner.write().unwrap();
    let keys: Vec<(Vec<u8>, Vec<u8>)> = write.dirty.drain().collect();
    keys.into_iter()
        .filter_map(|k| write.map.get(&k).map(|st| (k, *st)))
        .collect()
}

pub fn dirty_len(cache: &OffsetCache) -> usize {
    cache.inner.read().unwrap().dirty.len()
}

/// Re-validate a flush snapshot under the cache lock before it is
/// written: keep only entries that are STILL clean (no newer
/// ack/set-position re-marked them dirty after the snapshot was taken)
/// and whose group still exists. A dropped entry stays dirty and rides
/// the next round, so an old-snapshot batch can never land after a
/// newer write and drag the committed watermark backwards (which would
/// redeliver already-acked messages after a crash).
pub fn drop_superseded(
    cache: &OffsetCache,
    dirty: Vec<((Vec<u8>, Vec<u8>), GroupState)>,
) -> Vec<((Vec<u8>, Vec<u8>), GroupState)> {
    let inner = cache.inner.read().unwrap();
    dirty
        .into_iter()
        .filter(|(key, _)| !inner.dirty.contains(key) && inner.map.contains_key(key))
        .collect()
}

/// Build the flush batch: one kind-0x0E record per dirty group.
pub fn build_flush_batch(dirty: &[((Vec<u8>, Vec<u8>), GroupState)]) -> Option<WriteBatch> {
    if dirty.is_empty() {
        return None;
    }
    let mut batch = WriteBatch::default();
    for ((stream, group), st) in dirty {
        let Some(prefix) = model::stream_prefix(stream) else {
            continue;
        };
        let payload = GroupPayload {
            created_ms: st.created_ms,
            delivered_ms: st.delivered.ms,
            delivered_seq: st.delivered.seq,
            committed_ms: st.committed.ms,
            committed_seq: st.committed.seq,
        };
        batch.put(
            model::group_key(&prefix, stream, group),
            model::encode_group(&payload),
        );
    }
    Some(batch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_counts_and_clamps() {
        let c = new_cache();
        insert_new(
            &c,
            b"t/q0",
            b"g",
            GroupState {
                created_ms: 1,
                delivered: EntryId { ms: 10, seq: 0 },
                committed: EntryId { ms: 10, seq: 0 },
            },
        );
        // in-order acks: 2 of 3 beyond the watermark
        assert_eq!(
            ack(
                &c,
                b"t/q0",
                b"g",
                &[
                    EntryId { ms: 11, seq: 0 },
                    EntryId { ms: 12, seq: 0 },
                    EntryId { ms: 9, seq: 9 }
                ]
            ),
            Some(2)
        );
        // re-ack of old ids counts nothing
        assert_eq!(ack(&c, b"t/q0", b"g", &[EntryId { ms: 11, seq: 0 }]), Some(0));
        assert_eq!(dirty_len(&c), 1);
        let flushed = flush_dirty(&c);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.committed, EntryId { ms: 12, seq: 0 });
        assert_eq!(dirty_len(&c), 0);
        // unknown group: no crash, None
        assert_eq!(ack(&c, b"t/q0", b"nope", &[EntryId { ms: 1, seq: 0 }]), None);
    }

    #[test]
    fn set_position_and_remove() {
        let c = new_cache();
        insert_new(
            &c,
            b"t/q0",
            b"g",
            GroupState {
                created_ms: 1,
                delivered: EntryId { ms: 50, seq: 0 },
                committed: EntryId { ms: 40, seq: 0 },
            },
        );
        set_position(&c, b"t/q0", b"g", EntryId { ms: 5, seq: 5 });
        assert_eq!(dirty_len(&c), 1);
        let b = build_flush_batch(&flush_dirty(&c)).unwrap();
        assert!(!b.is_empty());
        remove_stream(&c, b"t/q0");
        assert_eq!(dirty_len(&c), 0);
        assert_eq!(ack(&c, b"t/q0", b"g", &[EntryId { ms: 9, seq: 0 }]), None);
    }

    #[test]
    fn drop_superseded_keeps_only_current_snapshots() {
        let c = new_cache();
        insert_new(
            &c,
            b"t/q0",
            b"g",
            GroupState {
                created_ms: 1,
                delivered: EntryId { ms: 10, seq: 0 },
                committed: EntryId { ms: 10, seq: 0 },
            },
        );
        // Flush round A snapshots committed=20; a newer ack (committed=30)
        // lands BEFORE A's write: A's stale snapshot must be dropped.
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 20, seq: 0 }]).unwrap();
        let round_a = flush_dirty(&c);
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 30, seq: 0 }]).unwrap();
        assert!(drop_superseded(&c, round_a).is_empty(), "superseded");
        assert_eq!(dirty_len(&c), 1, "the newer state stays dirty");
        // Round B (committed=30) is still current: it survives and keeps
        // the advanced watermark.
        let round_b = drop_superseded(&c, flush_dirty(&c));
        assert_eq!(round_b.len(), 1);
        assert_eq!(round_b[0].1.committed, EntryId { ms: 30, seq: 0 });
        // A group removed between snapshot and write is also dropped:
        // writing it would resurrect a deleted group record.
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 40, seq: 0 }]).unwrap();
        let round_c = flush_dirty(&c);
        remove_stream(&c, b"t/q0");
        assert!(drop_superseded(&c, round_c).is_empty(), "removed group");
    }
}
