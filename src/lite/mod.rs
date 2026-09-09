//! Lite Mode: RocketMQ 5.5 Lite semantics on Redis Streams verbs.
//!
//! Model: one **parent topic** owns dynamically created **LiteTopic
//! queues** named `parent/child`. `XADD parent/child` auto-creates the
//! stream; `XADD parent` picks a queue (see [`select`]). All X-commands
//! are cluster-whitelisted (node-local): the physical slot prefix is the
//! CRC16 slot of the PARENT name, so every queue of a topic shares one
//! contiguous key window.
//!
//! Lifecycle: `XGROUP CREATE` subscribes a consumer group, `XREAD/XREADGROUP
//! [BLOCK]` park on the shared WaitHub, `XACK` commits offsets (see
//! [`offset`]), `XIDLE` arms the uniform idle-TTL envelope so the existing
//! active-expiration loop reclaims whole streams family-wide.

pub mod ack;
pub mod append;
pub mod autoclaim;
pub mod claim;
pub mod entries;
pub mod group;
pub mod info;
pub mod model;
pub mod offset;
pub mod ordered;
pub mod park_wait;
pub mod pel;
pub mod pending;
pub mod range_rev;
pub mod read;
pub mod select;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rocksdb::WriteBatch;

use crate::ds::{codec, expire, latch};
use crate::hash;
use crate::monitor;
use crate::state;
use crate::store::ops;

/// Max bytes of one topic/queue name part.
pub const MAX_PART: usize = 64;

/// `[A-Za-z0-9._-]{1,64}`.
pub(crate) fn valid_part(p: &[u8]) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PART
        && p.iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// A validated Lite name: `parent/child` (a stream) or bare `parent`.
#[derive(Debug, PartialEq)]
pub enum TopicName {
    Parent(Vec<u8>),
    Stream(Vec<u8>, Vec<u8>),
}

pub fn parse_topic_name(name: &[u8]) -> Result<TopicName, String> {
    let text = String::from_utf8_lossy(name).to_string();
    match name.iter().position(|&b| b == b'/') {
        None => {
            if valid_part(name) {
                Ok(TopicName::Parent(name.to_vec()))
            } else {
                Err(format!("ERR invalid topic name '{text}'"))
            }
        }
        Some(i) => {
            let (parent, child) = (&name[..i], &name[i + 1..]);
            if valid_part(parent) && valid_part(child) {
                Ok(TopicName::Stream(parent.to_vec(), child.to_vec()))
            } else {
                Err(format!("ERR invalid stream name '{text}'"))
            }
        }
    }
}

/// Process-lifetime Lite counters (approximate across restarts; exported
/// via `XINFO LITE` and the `rdb_lite_*` metrics).
#[derive(Default)]
pub struct Stats {
    pub messages: AtomicU64,
    pub acks: AtomicU64,
    pub streams_live: AtomicI64,
    pub streams_reaped: AtomicU64,
}

pub fn stat_bump(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}

/// Default ordered-group ownership lease: a queue whose owner has been
/// silent this long is considered abandoned and migrates to the next
/// asking consumer (idle takeover; XCLAIM/XAUTOCLAIM bypass it via
/// min-idle-time).
pub const DEFAULT_LEASE_MS: u64 = 30_000;

/// Lite runtime state hung off `state::Shared`.
pub struct Runtime {
    pub offsets: offset::OffsetCache,
    /// Ordered-group queue ownership (see `ordered`); in-memory by
    /// design: ownership is process-local (a restart drops every
    /// connection, so pre-restart zombies cannot exist) and a lazy
    /// re-acquire rebuilds it on the first `>` read.
    pub owners: Mutex<ordered::OwnerMap>,
    /// Ownership lease window (test hook; see DEFAULT_LEASE_MS).
    lease_ms: std::sync::atomic::AtomicU64,
    /// Per-parent round-robin cursors.
    pub picks: Mutex<HashMap<Vec<u8>, u64>>,
    /// Consumers already registered this process, (stream, group,
    /// consumer) raw bytes: delivery skips the registry-key rewrite once
    /// the name is known (the key survives on disk).
    pub consumers: Mutex<HashSet<pel::ConsumerId>>,
    /// Deferred orphan sweeps: streams whose family records were deleted
    /// by a NON-command path (XIDLE active-expire reap, lazy idle purge,
    /// DEL/EXPIRE of a stream key) and still need the latched sweep of
    /// [`reap_stream`]. Pairs of (slot prefix, stream name).
    reaps: Mutex<Vec<(Vec<u8>, Vec<u8>)>>,
    pub stats: Stats,
}

impl Runtime {
    /// `stream`'s family records were just deleted by a non-command
    /// path: drop its cached group states NOW -- any flush round that
    /// validates after this drops the entry in `drop_superseded` -- and
    /// queue the latched sweep that removes orphans a round already
    /// past validation may still write (see [`reap_stream`]).
    /// Ordered-group lease window (tests shrink it to exercise idle
    /// takeover without sleeping out the production default).
    pub fn set_lease_ms(&self, ms: u64) {
        self.lease_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn lease_ms(&self) -> u64 {
        self.lease_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn stream_reaped(&self, prefix: &[u8], stream: &[u8]) {
        offset::remove_stream(&self.offsets, stream);
        ordered::drop_stream(&self.owners, stream);
        self.reaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((prefix.to_vec(), stream.to_vec()));
    }

    /// Sweeps queued and awaiting the next [`drain_reaps`] (tests).
    pub fn pending_reaps(&self) -> usize {
        self.reaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

impl Runtime {
    /// First-sight check for the consumer registry: `false` = not yet
    /// known this process (the caller writes the registry key once),
    /// `true` = already registered -- and remembered either way.
    pub fn ensure_consumer(&self, stream: &[u8], group: &[u8], consumer: &[u8]) -> bool {
        let mut set = self
            .consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.insert((stream.to_vec(), group.to_vec(), consumer.to_vec()))
    }

    /// Forget one consumer (XGROUP DELCONSUMER).
    pub fn forget_consumer(&self, stream: &[u8], group: &[u8], consumer: &[u8]) {
        let mut set = self
            .consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.remove(&(stream.to_vec(), group.to_vec(), consumer.to_vec()));
    }

    /// Forget every consumer of a group (XGROUP DESTROY wiped its window).
    pub fn forget_group(&self, stream: &[u8], group: &[u8]) {
        let mut set = self
            .consumers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.retain(|(s, g, _)| !(s == stream && g == group));
    }
}

pub fn new_runtime() -> Runtime {
    Runtime {
        offsets: offset::new_cache(),
        owners: Mutex::new(HashMap::new()),
        lease_ms: std::sync::atomic::AtomicU64::new(DEFAULT_LEASE_MS),
        picks: Mutex::new(HashMap::new()),
        consumers: Mutex::new(HashSet::new()),
        reaps: Mutex::new(Vec::new()),
        stats: Stats::default(),
    }
}

/// One offset-flush round, shared by the 200ms background loop and the
/// E1 shutdown path (`flush_offsets_once`): lock-swap the dirty set, then
/// re-validate against the CURRENT dirty state before writing -- entries
/// superseded by a newer ack since the snapshot stay dirty and ride the
/// next round, so a late old batch can never lower the committed watermark
/// already on disk -- then one async batched fsync.
///
/// The whole round runs under the per-stream latches of every dirty
/// stream (derived from the parent part of the stream name -- the bytes
/// before the first `/`, or the whole name), acquired in byte-sorted KEY
/// order like the RENAME convention and held across the awaited write:
/// without them, the background round and the shutdown flush could
/// commit out of order and regress the on-disk watermark, and a round
/// that passed `drop_superseded` could still land after XGROUP DESTROY's
/// commit and resurrect the destroyed group record.
/// Latch key of one stream's flush window: the meta key under the
/// PARENT-derived slot prefix (bytes before the first `/`, or the whole
/// name). One derivation shared by flush rounds, FLUSHDB's wipe and the
/// deferred orphan sweep, so all three serialize on the same lock.
pub fn stream_latch_key(stream: &[u8]) -> Vec<u8> {
    let parent = match stream.iter().position(|&b| b == b'/') {
        Some(i) => &stream[..i],
        None => stream,
    };
    model::meta_key(&hash::slot_with_prefix(parent).1, stream)
}

/// Byte-sorted, deduplicated latch keys of every currently-dirty stream
/// (deadlock avoidance when two latch-taking operations overlap).
pub fn dirty_latch_keys(shared: &state::Shared) -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = offset::dirty_keys(&shared.lite.offsets)
        .into_iter()
        .map(|(stream, _)| stream_latch_key(&stream))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// Deferred half of [`Runtime::stream_reaped`]: under the stream's flush
/// latch, drop the cached state again (idempotent) and range-delete the
/// stream's whole group-record window. The sweep closes the last race:
/// a flush round that already passed `drop_superseded` when the family
/// delete landed can still commit its group records AFTER that delete,
/// and only deleting under the same latch (rounds hold it across their
/// write) is ordered after every in-flight round.
///
/// Guard: the sweep only fires while the stream is still gone (or still
/// idle-expired). A client that recreated the stream (XADD + XGROUP
/// CREATE) between the family delete and this sweep owns the window now
/// and must not lose its records.
pub async fn reap_stream(shared: &state::Shared, prefix: &[u8], stream: &[u8]) {
    let _guard = latch::lock(&shared.latch, &stream_latch_key(stream)).await;
    offset::remove_stream(&shared.lite.offsets, stream);
    let meta = model::meta_key(prefix, stream);
    let stale = match ops::get_physical(&shared.store, &meta) {
        // Still gone: any group record in the window is an orphan.
        Ok(None) => true,
        // Revived with the idle deadline still due: stale revival.
        Ok(Some(raw)) => {
            let (expire_ms, _) = codec::decode_envelope(&raw);
            expire::is_expired(expire_ms, expire::now_ms())
        }
        // Cannot prove staleness: never delete blind.
        Err(_) => false,
    };
    if stale {
        let mut batch = WriteBatch::default();
        let (lower, upper) = model::group_window(prefix, stream);
        batch.delete_range(lower, upper);
        if let Err(e) = ops::batch_write_async(Arc::clone(&shared.store), batch).await {
            eprintln!(
                "[lite] orphan sweep failed for {}: {e}",
                String::from_utf8_lossy(stream)
            );
        }
    }
}

/// Drain the deferred reap queue. The background offset loop calls this
/// on every 200ms tick and the active-expire loop after every sampler
/// round; concurrent drains are safe -- the queue is swapped under its
/// lock, reaping is idempotent and duplicates collapse.
pub async fn drain_reaps(shared: &state::Shared) {
    let queued = std::mem::take(
        &mut *shared
            .lite
            .reaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    let mut seen = HashSet::new();
    for (prefix, stream) in queued {
        if seen.insert(stream.clone()) {
            reap_stream(shared, &prefix, &stream).await;
        }
    }
}

pub async fn flush_offsets_once(shared: &Arc<state::Shared>) -> Result<(), String> {
    let latch_keys = dirty_latch_keys(shared);
    let mut guards = Vec::with_capacity(latch_keys.len());
    for key in &latch_keys {
        guards.push(crate::ds::latch::lock(&shared.latch, key).await);
    }
    let dirty = offset::flush_dirty(&shared.lite.offsets);
    let dirty = offset::drop_superseded(&shared.lite.offsets, dirty);
    monitor::set_lite_offset_dirty(&shared.monitor, dirty.len() as f64);
    if let Some(batch) = offset::build_flush_batch(&dirty) {
        ops::batch_write_async(Arc::clone(&shared.store), batch).await?;
    }
    drop(guards);
    Ok(())
}

/// Background loop (normal mode only): every 200ms flush dirty group
/// offsets (one async batched fsync per round) and refresh gauges.
pub fn spawn_background(shared: Arc<state::Shared>) {
    const PERIOD: Duration = Duration::from_millis(200);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PERIOD);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            if let Err(e) = flush_offsets_once(&shared).await {
                eprintln!("[lite] offset flush failed: {e}");
            }
            monitor::set_lite_streams(
                &shared.monitor,
                shared.lite.stats.streams_live.load(Ordering::Relaxed) as f64,
                shared.lite.stats.streams_reaped.load(Ordering::Relaxed) as f64,
            );
            // Unacked-pending backlog across every cached group (exact:
            // reloaded from the PEL window at first load, then delta-kept).
            monitor::set_lite_backlog(
                &shared.monitor,
                offset::total_pending(&shared.lite.offsets) as f64,
            );
            // Deferred orphan sweeps queued by non-command delete paths
            // (XIDLE reaps, lazy idle purges) since the last tick.
            drain_reaps(&shared).await;
        }
    });
}
