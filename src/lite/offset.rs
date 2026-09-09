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
    /// Cache-only unacked-pending (PEL) count; NOT part of the persisted
    /// GroupPayload. Reloaded exactly from the kind-0x0F window on the
    /// first load after a restart, then maintained by delivery (+n),
    /// XACK (-n) and XGROUP DELCONSUMER (-k) deltas.
    pub pending: u64,
    /// Ordered-group flag (queue-exclusive ownership + in-flight cap);
    /// persisted in the GroupPayload so it survives restarts.
    pub ordered: bool,
    /// Per-queue unacked-entry cap; normalized to >= 1 for ordered
    /// groups (0 only on unordered ones).
    pub inflight_max: u64,
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
    // Exact pending backlog after a restart: one PEL scan per group per
    // process (loads are cached), then deltas keep it exact.
    let pending = super::pel::count_pend(store, prefix, stream, group).unwrap_or(0);
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
        pending,
        ordered: p.ordered,
        inflight_max: super::model::normalize_inflight(p.ordered, p.inflight_max),
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

/// XACK, Kafka committed-offset semantics: the committed watermark is a
/// POSITION that only ever advances over a CONTIGUOUS acked prefix.
/// `head_after` is the first pending row that survives this ack beyond
/// the old watermark (None = no survivor up to the max acked id,
/// computed by `pel::head_after_ack` under the stream latch); the
/// watermark stops right below it, so a restart can never resume PAST
/// an unacked entry (the old `max(committed, id)` skip-commit could
/// lose delivered-but-unacked messages). Ids acked beyond a gap still
/// count in the reply and stay acked on the PEL side -- the tail is
/// redelivered later (at-least-once duplicates, never loss). Returns
/// how many of `ids` were beyond the old watermark.
pub fn ack(
    cache: &OffsetCache,
    stream: &[u8],
    group: &[u8],
    ids: &[EntryId],
    head_after: Option<EntryId>,
) -> Option<usize> {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_vec(), group.to_vec());
    let st = write.map.get_mut(&key)?;
    let old = st.committed;
    let mut count = 0usize;
    for id in ids {
        if *id > old {
            count += 1;
        }
    }
    // Contiguous-prefix candidate: the largest acked id with no
    // surviving pending row between the old watermark and itself.
    let max_acked = ids.iter().max().copied().unwrap_or(old);
    let candidate = match head_after {
        None => max_acked,
        // Ids at/after the gap head cannot advance the position; ids
        // strictly below it are contiguous by construction (the head
        // IS the first survivor).
        Some(h) => ids
            .iter()
            .copied()
            .filter(|id| *id < h)
            .max()
            .unwrap_or(old),
    }
    .max(old);
    if candidate > st.committed {
        st.committed = candidate;
    }
    if st.committed > old {
        if st.committed > st.delivered {
            st.delivered = st.committed;
        }
        write.dirty.insert(key);
    }
    Some(count)
}

/// Adjust the cached pending backlog of one group (delivery +n, ack
/// / DELCONSUMER purges -n). Signed so one call site covers both; the
/// value is a counter, never a watermark -- it must NOT clamp delivery.
pub fn bump_pending(cache: &OffsetCache, stream: &[u8], group: &[u8], delta: i64) {
    let mut write = cache.inner.write().unwrap();
    let key = (stream.to_vec(), group.to_vec());
    if let Some(st) = write.map.get_mut(&key) {
        st.pending = if delta >= 0 {
            st.pending.saturating_add(delta as u64)
        } else {
            st.pending.saturating_sub(delta.unsigned_abs())
        };
    }
}

/// Total unacked-pending across every cached group (rdb_lite_backlog).
pub fn total_pending(cache: &OffsetCache) -> u64 {
    cache
        .inner
        .read()
        .unwrap()
        .map
        .values()
        .map(|st| st.pending)
        .sum()
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
/// Snapshot of dirty group states drained for one flush round:
/// `((stream, group), state)`.
pub type DirtySnapshot = Vec<((Vec<u8>, Vec<u8>), GroupState)>;

pub fn flush_dirty(cache: &OffsetCache) -> DirtySnapshot {
    let mut write = cache.inner.write().unwrap();
    let keys: Vec<(Vec<u8>, Vec<u8>)> = write.dirty.drain().collect();
    keys.into_iter()
        .filter_map(|k| write.map.get(&k).map(|st| (k, *st)))
        .collect()
}

/// Drop the whole cache, dirty set included (FLUSHDB: every stream is
/// being wiped, so every cached group state is stale; leaving entries
/// dirty would let the next flush round resurrect orphan group records
/// onto a wiped keyspace). Safe at any time: loads are read-through, so
/// the cache repopulates from disk on demand.
pub fn clear_all(cache: &OffsetCache) {
    let mut write = cache.inner.write().unwrap();
    write.map.clear();
    write.dirty.clear();
}

pub fn dirty_len(cache: &OffsetCache) -> usize {
    cache.inner.read().unwrap().dirty.len()
}

/// Peek the dirty `(stream, group)` keys WITHOUT draining (read lock):
/// lets a flush round derive the per-stream latches to hold BEFORE the
/// dirty set is swapped out, so the snapshot and the write are
/// serialized against XGROUP DESTROY and other flush rounds.
pub fn dirty_keys(cache: &OffsetCache) -> Vec<(Vec<u8>, Vec<u8>)> {
    cache.inner.read().unwrap().dirty.iter().cloned().collect()
}

/// Re-validate a flush snapshot under the cache lock before it is
/// written: keep only entries that are STILL clean (no newer
/// ack/set-position re-marked them dirty after the snapshot was taken)
/// and whose group still exists. A dropped entry stays dirty and rides
/// the next round, so an old-snapshot batch can never land after a
/// newer write and drag the committed watermark backwards (which would
/// redeliver already-acked messages after a crash).
pub fn drop_superseded(cache: &OffsetCache, dirty: DirtySnapshot) -> DirtySnapshot {
    let inner = cache.inner.read().unwrap();
    dirty
        .into_iter()
        .filter(|(key, _)| !inner.dirty.contains(key) && inner.map.contains_key(key))
        .collect()
}

/// Build the flush batch: one kind-0x0E record per dirty group.
pub fn build_flush_batch(dirty: &DirtySnapshot) -> Option<WriteBatch> {
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
            ordered: st.ordered,
            inflight_max: st.inflight_max,
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

    fn st(committed_ms: u64) -> GroupState {
        GroupState {
            created_ms: 1,
            delivered: EntryId {
                ms: committed_ms,
                seq: 0,
            },
            committed: EntryId {
                ms: committed_ms,
                seq: 0,
            },
            pending: 0,
            ordered: false,
            inflight_max: 0,
        }
    }

    #[test]
    fn ack_counts_and_clamps() {
        let c = new_cache();
        insert_new(&c, b"t/q0", b"g", st(10));
        // in-order acks (no surviving pending row): 2 of 3 beyond the
        // watermark, committed advances to the max acked id.
        assert_eq!(
            ack(
                &c,
                b"t/q0",
                b"g",
                &[
                    EntryId { ms: 11, seq: 0 },
                    EntryId { ms: 12, seq: 0 },
                    EntryId { ms: 9, seq: 9 }
                ],
                None
            ),
            Some(2)
        );
        // re-ack of old ids counts nothing
        assert_eq!(
            ack(&c, b"t/q0", b"g", &[EntryId { ms: 11, seq: 0 }], None),
            Some(0)
        );
        assert_eq!(dirty_len(&c), 1);
        let flushed = flush_dirty(&c);
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].1.committed, EntryId { ms: 12, seq: 0 });
        assert_eq!(dirty_len(&c), 0);
        // unknown group: no crash, None
        assert_eq!(
            ack(&c, b"t/q0", b"nope", &[EntryId { ms: 1, seq: 0 }], None),
            None
        );
    }

    #[test]
    fn ack_stops_at_the_first_surviving_pending_row() {
        let c = new_cache();
        insert_new(&c, b"t/q0", b"g", st(10));
        // Out-of-order ack: 12-0 acked while 11-0 stays pending. The
        // reply still counts it, but the watermark must NOT skip the
        // gap -- Kafka committed-offset semantics (a skip would make a
        // restart resume past an unacked message: silent loss).
        assert_eq!(
            ack(
                &c,
                b"t/q0",
                b"g",
                &[EntryId { ms: 12, seq: 0 }],
                Some(EntryId { ms: 11, seq: 0 })
            ),
            Some(1)
        );
        assert!(
            flush_dirty(&c).is_empty(),
            "the gap froze the position: nothing new to persist"
        );
        // Gap closes: 11-0 acked, next survivor is 13-0. The position
        // moves to the contiguous prefix max -- 11-0. The earlier acked
        // 12-0 is NOT remembered (no acked-set state): it lies in the
        // redelivered tail, at-least-once duplicates by contract.
        assert_eq!(
            ack(
                &c,
                b"t/q0",
                b"g",
                &[EntryId { ms: 11, seq: 0 }],
                Some(EntryId { ms: 13, seq: 0 })
            ),
            Some(1)
        );
        assert_eq!(flush_dirty(&c)[0].1.committed, EntryId { ms: 11, seq: 0 });
        // All acked below the max acked id: position = max acked.
        assert_eq!(
            ack(&c, b"t/q0", b"g", &[EntryId { ms: 14, seq: 0 }], None),
            Some(1)
        );
        assert_eq!(flush_dirty(&c)[0].1.committed, EntryId { ms: 14, seq: 0 });
        // A gap with NO acked id below it moves nothing (the ack still
        // counts -- the id IS acked on the PEL side, just not committed).
        assert_eq!(
            ack(
                &c,
                b"t/q0",
                b"g",
                &[EntryId { ms: 20, seq: 0 }],
                Some(EntryId { ms: 15, seq: 0 })
            ),
            Some(1)
        );
        assert!(flush_dirty(&c).is_empty());
    }

    #[test]
    fn set_position_and_remove() {
        let c = new_cache();
        insert_new(&c, b"t/q0", b"g", st(10));
        set_position(&c, b"t/q0", b"g", EntryId { ms: 5, seq: 5 });
        assert_eq!(dirty_len(&c), 1);
        // A rewind re-dirties the entry (delivered moves back with it).
        let flushed = flush_dirty(&c);
        assert_eq!(flushed[0].1.delivered, EntryId { ms: 5, seq: 5 });
        remove_stream(&c, b"t/q0");
        assert_eq!(dirty_len(&c), 0);
    }

    #[test]
    fn drop_superseded_keeps_current_states_only() {
        let c = new_cache();
        insert_new(&c, b"t/q0", b"g", st(10));
        // Flush round A snapshots committed=20; a NEWER ack lands
        // BEFORE A's write: A's stale snapshot must be dropped.
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 20, seq: 0 }], None).unwrap();
        let round_a = flush_dirty(&c);
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 30, seq: 0 }], None).unwrap();
        assert!(drop_superseded(&c, round_a).is_empty(), "superseded");
        assert_eq!(dirty_len(&c), 1, "the newer state stays dirty");
        // Round B (committed=30) is still current: it survives and keeps
        // the advanced watermark.
        let round_b = drop_superseded(&c, flush_dirty(&c));
        assert_eq!(round_b.len(), 1);
        assert_eq!(round_b[0].1.committed, EntryId { ms: 30, seq: 0 });
        // A group removed between snapshot and write is also dropped:
        // writing it would resurrect a deleted group record.
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 40, seq: 0 }], None).unwrap();
        let round_c = flush_dirty(&c);
        remove_stream(&c, b"t/q0");
        assert!(drop_superseded(&c, round_c).is_empty(), "removed group");
    }

    #[test]
    fn dirty_keys_peek_does_not_drain() {
        let c = new_cache();
        insert_new(&c, b"t/q0", b"g", st(10));
        // Nothing dirty yet: the peek is empty.
        assert!(dirty_keys(&c).is_empty());
        ack(&c, b"t/q0", b"g", &[EntryId { ms: 20, seq: 0 }], None).unwrap();
        // Peek lists the dirty (stream, group) pair without draining --
        // the flush round derives its latch keys from this snapshot.
        assert_eq!(dirty_keys(&c), vec![(b"t/q0".to_vec(), b"g".to_vec())]);
        assert_eq!(dirty_len(&c), 1, "peek must not clear the dirty set");
        // Draining via flush_dirty empties the peek too.
        let drained = flush_dirty(&c);
        assert_eq!(drained.len(), 1);
        assert!(dirty_keys(&c).is_empty());
    }
}
