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
pub mod entries;
pub mod group;
pub mod info;
pub mod model;
pub mod offset;
pub mod read;
pub mod select;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    pub stats: Stats,
}

pub fn new_runtime() -> Runtime {
    Runtime {
        offsets: offset::new_cache(),
        picks: Mutex::new(HashMap::new()),
        stats: Stats::default(),
    }
}

/// Background loop (normal mode only): every 200ms flush dirty group
/// offsets (lock-swap, then one async batched fsync) and refresh gauges.
pub fn spawn_background(shared: Arc<state::Shared>) {
    const PERIOD: Duration = Duration::from_millis(200);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PERIOD);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            let dirty = offset::flush_dirty(&shared.lite.offsets);
            // Re-validate against the CURRENT dirty state before writing:
            // entries superseded by a newer ack since the snapshot stay
            // dirty and ride the next round, so a late old batch can
            // never lower the committed watermark already on disk.
            let dirty = offset::drop_superseded(&shared.lite.offsets, dirty);
            monitor::set_lite_offset_dirty(&shared.monitor, dirty.len() as f64);
            if let Some(batch) = offset::build_flush_batch(&dirty) {
                if let Err(e) = ops::batch_write_async(Arc::clone(&shared.store), batch).await {
                    eprintln!("[lite] offset flush failed: {e}");
                }
            }
            monitor::set_lite_streams(
                &shared.monitor,
                shared.lite.stats.streams_live.load(Ordering::Relaxed) as f64,
                shared.lite.stats.streams_reaped.load(Ordering::Relaxed) as f64,
            );
        }
    });
}
