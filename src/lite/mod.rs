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
pub mod park_wait;
pub mod pel;
pub mod pending;
pub mod read;
pub mod select;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Lite runtime state hung off `state::Shared`.
pub struct Runtime {
    pub offsets: offset::OffsetCache,
    /// Per-parent round-robin cursors.
    pub picks: Mutex<HashMap<Vec<u8>, u64>>,
    /// Consumers already registered this process, (stream, group,
    /// consumer) raw bytes: delivery skips the registry-key rewrite once
    /// the name is known (the key survives on disk).
    pub consumers: Mutex<HashSet<pel::ConsumerId>>,
    pub stats: Stats,
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
        picks: Mutex::new(HashMap::new()),
        consumers: Mutex::new(HashSet::new()),
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
pub async fn flush_offsets_once(shared: &Arc<state::Shared>) -> Result<(), String> {
    // Distinct latch keys of the dirty streams, byte-sorted (deadlock
    // avoidance when two rounds overlap on several streams).
    let mut latch_keys: Vec<Vec<u8>> = offset::dirty_keys(&shared.lite.offsets)
        .into_iter()
        .map(|(stream, _)| {
            let parent = match stream.iter().position(|&b| b == b'/') {
                Some(i) => &stream[..i],
                None => &stream[..],
            };
            model::meta_key(&hash::slot_with_prefix(parent).1, &stream)
        })
        .collect();
    latch_keys.sort();
    latch_keys.dedup();
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
        }
    });
}
