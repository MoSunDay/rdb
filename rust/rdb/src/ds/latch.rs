//! Sharded per-key latches serializing read-modify-write sequences.
//!
//! Free-function API over plain data carriers (no methods with business
//! logic): [`lock`] hands out a RAII [`KeyGuard`] for one physical root
//! key; the guard releases the key on drop.
//!
//! Deadlock rule: operations touching MULTIPLE keys must sort the root
//! keys bytewise and lock them in that order (see the RENAME handler);
//! single-key operations are always safe.
//!
//! Growth: entries are never pruned -- the map grows to the number of
//! distinct latched roots (bounded by live keys, one `Arc` each). The
//! simpler-safe choice; opportunistic pruning risks a race where a waiter
//! is dropped along with the entry it is queued on.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

/// Shard count; 256 keeps shard-mutex contention negligible.
pub const SHARDS: usize = 256;

/// Per-key wait cell: the held flag lives INSIDE the mutex so waiters and
/// the releasing guard share one source of truth.
struct LatchCell {
    state: Mutex<bool>, // true = key held
    cv: Condvar,
}

/// Latch table: fixed shards of root-key -> cell maps.
pub struct Latch {
    shards: Vec<Mutex<HashMap<Vec<u8>, Arc<LatchCell>>>>,
}

impl Latch {
    pub fn new() -> Latch {
        Latch {
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
        }
    }
}

impl Default for Latch {
    fn default() -> Self {
        Latch::new()
    }
}

/// RAII holder of one latched key; dropping it releases the key and wakes
/// the next waiter.
pub struct KeyGuard {
    cell: Arc<LatchCell>,
}

impl Drop for KeyGuard {
    fn drop(&mut self) {
        let mut held = self
            .cell
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = false;
        drop(held);
        self.cell.cv.notify_one();
    }
}

/// FNV-1a over the key bytes, folded into the shard range.
pub fn shard_of(key: &[u8]) -> usize {
    let mut hash: u32 = 0x811c9dc5;
    for &b in key {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x01000193);
    }
    (hash as usize) % SHARDS
}

/// Acquire (blocking) the latch for `key`.
pub fn lock(latch: &Latch, key: &[u8]) -> KeyGuard {
    let cell = {
        let mut shard = latch.shards[shard_of(key)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shard
            .entry(key.to_vec())
            .or_insert_with(|| {
                Arc::new(LatchCell {
                    state: Mutex::new(false),
                    cv: Condvar::new(),
                })
            })
            .clone()
    };
    let mut held = cell
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Sleep while ANOTHER holder owns the key; the releaser's notify_one
    // wakes exactly one waiter.
    while *held {
        held = cell
            .cv
            .wait(held)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    *held = true;
    drop(held);
    KeyGuard { cell }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn shard_of_is_stable_and_bounded() {
        assert_eq!(shard_of(b""), shard_of(b""));
        assert_eq!(shard_of(b"70/k"), shard_of(b"70/k"));
        assert_ne!(shard_of(b"70/k"), shard_of(b"70/j"));
        for k in ["a", "70/\x02\x00\x00\x00\x01k", "long root key bytes"] {
            assert!(shard_of(k.as_bytes()) < SHARDS);
        }
    }

    #[test]
    fn same_key_serializes_different_keys_do_not() {
        let latch = Latch::new();
        {
            let _g1 = lock(&latch, b"70/k");
            {
                let _g2 = lock(&latch, b"71/other");
                // different roots: both held simultaneously without deadlock
            }
            // re-locking the SAME key here would self-deadlock; asserted
            // across threads below instead.
        }
        // dropped guards release: a fresh lock succeeds immediately.
        let _g3 = lock(&latch, b"70/k");
    }

    #[test]
    fn mutual_exclusion_across_threads() {
        let latch = Arc::new(Latch::new());
        let counter = Arc::new(Mutex::new(0usize));
        let inside = Arc::new(Mutex::new(0usize));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let (l, c, i) = (
                Arc::clone(&latch),
                Arc::clone(&counter),
                Arc::clone(&inside),
            );
            handles.push(thread::spawn(move || {
                for _ in 0..200 {
                    let _g = lock(&l, b"70/hot");
                    {
                        let mut seen = i.lock().unwrap();
                        *seen += 1;
                        assert_eq!(*seen, 1, "two threads inside the latch");
                    }
                    *c.lock().unwrap() += 1;
                    {
                        let mut seen = i.lock().unwrap();
                        *seen -= 1;
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*counter.lock().unwrap(), 8 * 200);
    }
}
