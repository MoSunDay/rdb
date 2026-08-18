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

struct Inner {
    map: HashMap<(String, String), GroupState>,
    dirty: HashSet<(String, String)>,
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
    stream: &str,
    group: &str,
) -> Result<Option<GroupState>, String> {
    {
        let read = cache.inner.read().unwrap();
        if let Some(st) = read.map.get(&(stream.to_string(), group.to_string())) {
            return Ok(Some(*st));
        }
    }
    let loaded = model::read_group(store, prefix, stream.as_bytes(), group.as_bytes())?;
    let mut write = cache.inner.write().unwrap();
    // Double-check: a concurrent loader may have won the race.
    if let Some(st) = write.map.get(&(stream.to_string(), group.to_string())) {
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
    write
        .map
        .insert((stream.to_string(), group.to_string()), st);
    Ok(Some(st))
}

/// XGROUP CREATE path: insert a fresh state (clean -- CREATE persists it).
pub fn insert_new(cache: &OffsetCache, stream: &str, group: &str, st: GroupState) {
    cache
        .inner
        .write()
        .unwrap()
        .map
        .insert((stream.to_string(), group.to_string()), st);
}

/// XREADGROUP `>`: advance the memory-only delivery watermark.
pub fn advance_delivered(cache: &OffsetCache, stream: &str, group: &str, id: EntryId) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_string(), group.to_string());
    if let Some(st) = write.map.get_mut(&key) {
        if id > st.delivered {
            st.delivered = id;
        }
    }
}

/// XACK: `committed = max(committed, max id)`; returns how many of `ids`
/// were beyond the old watermark (the "newly acked" count).
pub fn ack(cache: &OffsetCache, stream: &str, group: &str, ids: &[EntryId]) -> Option<usize> {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_string(), group.to_string());
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
pub fn set_position(cache: &OffsetCache, stream: &str, group: &str, id: EntryId) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_string(), group.to_string());
    if let Some(st) = write.map.get_mut(&key) {
        st.delivered = id;
        st.committed = id;
        write.dirty.insert(key);
    }
}

pub fn remove_group(cache: &OffsetCache, stream: &str, group: &str) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_string(), group.to_string());
    write.map.remove(&key);
    write.dirty.remove(&key);
}

/// Drop every cached group of `stream` (XGROUP-less re-create of a stream).
pub fn remove_stream(cache: &OffsetCache, stream: &str) {
    let mut write = cache.inner.write().unwrap();
    write.map.retain(|(s, _), _| s != stream);
    write.dirty.retain(|(s, _)| s != stream);
}

/// Snapshot + clear the dirty set (batch construction happens off-lock).
pub fn flush_dirty(cache: &OffsetCache) -> Vec<((String, String), GroupState)> {
    let mut write = cache.inner.write().unwrap();
    let keys: Vec<(String, String)> = write.dirty.drain().collect();
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
    dirty: Vec<((String, String), GroupState)>,
) -> Vec<((String, String), GroupState)> {
    let inner = cache.inner.read().unwrap();
    dirty
        .into_iter()
        .filter(|(key, _)| !inner.dirty.contains(key) && inner.map.contains_key(key))
        .collect()
}

/// Build the flush batch: one kind-0x0E record per dirty group.
pub fn build_flush_batch(dirty: &[((String, String), GroupState)]) -> Option<WriteBatch> {
    if dirty.is_empty() {
        return None;
    }
    let mut batch = WriteBatch::default();
    for ((stream, group), st) in dirty {
        let Some(prefix) = model::stream_prefix(stream.as_bytes()) else {
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
            model::group_key(&prefix, stream.as_bytes(), group.as_bytes()),
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
            "t/q0",
            "g",
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
                "t/q0",
                "g",
                &[
                    EntryId { ms: 11, seq: 0 },
                    EntryId { ms: 12, seq: 0 },
                    EntryId { ms: 9, seq: 9 }
                ]
            ),
            Some(2)
        );
        // re-ack of old ids counts nothing
        assert_eq!(ack(&c, "t/q0", "g", &[EntryId { ms: 11, seq: 0 }]), Some(0));
        assert_eq!(dirty_len(&c), 1);
        let flushed = flush_dirty(&c);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.committed, EntryId { ms: 12, seq: 0 });
        assert_eq!(dirty_len(&c), 0);
        // unknown group: no crash, None
        assert_eq!(ack(&c, "t/q0", "nope", &[EntryId { ms: 1, seq: 0 }]), None);
    }

    #[test]
    fn set_position_and_remove() {
        let c = new_cache();
        insert_new(
            &c,
            "t/q0",
            "g",
            GroupState {
                created_ms: 1,
                delivered: EntryId { ms: 50, seq: 0 },
                committed: EntryId { ms: 40, seq: 0 },
            },
        );
        set_position(&c, "t/q0", "g", EntryId { ms: 5, seq: 5 });
        assert_eq!(dirty_len(&c), 1);
        let b = build_flush_batch(&flush_dirty(&c)).unwrap();
        assert!(!b.is_empty());
        remove_stream(&c, "t/q0");
        assert_eq!(dirty_len(&c), 0);
        assert_eq!(ack(&c, "t/q0", "g", &[EntryId { ms: 9, seq: 0 }]), None);
    }

    #[test]
    fn drop_superseded_keeps_only_current_snapshots() {
        let c = new_cache();
        insert_new(
            &c,
            "t/q0",
            "g",
            GroupState {
                created_ms: 1,
                delivered: EntryId { ms: 10, seq: 0 },
                committed: EntryId { ms: 10, seq: 0 },
            },
        );
        // Flush round A snapshots committed=20; a newer ack (committed=30)
        // lands BEFORE A's write: A's stale snapshot must be dropped.
        ack(&c, "t/q0", "g", &[EntryId { ms: 20, seq: 0 }]).unwrap();
        let round_a = flush_dirty(&c);
        ack(&c, "t/q0", "g", &[EntryId { ms: 30, seq: 0 }]).unwrap();
        assert!(drop_superseded(&c, round_a).is_empty(), "superseded");
        assert_eq!(dirty_len(&c), 1, "the newer state stays dirty");
        // Round B (committed=30) is still current: it survives and keeps
        // the advanced watermark.
        let round_b = drop_superseded(&c, flush_dirty(&c));
        assert_eq!(round_b.len(), 1);
        assert_eq!(round_b[0].1.committed, EntryId { ms: 30, seq: 0 });
        // A group removed between snapshot and write is also dropped:
        // writing it would resurrect a deleted group record.
        ack(&c, "t/q0", "g", &[EntryId { ms: 40, seq: 0 }]).unwrap();
        let round_c = flush_dirty(&c);
        remove_stream(&c, "t/q0");
        assert!(drop_superseded(&c, round_c).is_empty(), "removed group");
    }
}
