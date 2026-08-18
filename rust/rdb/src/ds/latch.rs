//! Sharded per-key latches serializing read-modify-write sequences.
//!
//! Free-function API over plain data carriers (no methods with business
//! logic): [`lock`] hands out a RAII [`KeyGuard`] for one physical root
//! key; the guard releases the key on drop.
//!
//! ASYNC: each key is a `tokio::sync::Semaphore` with one permit, so a
//! waiter parks a TASK (not an OS thread) and the guard's drop wakes the
//! next waiter FIFO. This is load-bearing: handlers hold guards across
//! `.await` points (e.g. the commit after a blocked pop), which under a
//! Condvar latch would pin tokio worker threads.
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
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Shard count; 256 keeps shard-mutex contention negligible.
pub const SHARDS: usize = 256;

/// Latch table: fixed shards of root-key -> semaphore maps. One permit
/// per key semaphore = at most one holder at a time.
pub struct Latch {
    shards: Vec<Mutex<HashMap<Vec<u8>, Arc<Semaphore>>>>,
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

/// RAII holder of one latched key; dropping the permit releases the key
/// and hands the permit to the next queued waiter (semaphores are
/// FIFO-fair and wake on release). Pure data carrier: the token's own
/// Drop does the work, no business logic here.
pub struct KeyGuard {
    _permit: OwnedSemaphorePermit,
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

/// Acquire (async) the latch for `key`. The shard mutex is held only for
/// the map lookup; the semaphore wait parks AFTER it is released, so a
/// blocked waiter never stalls unrelated keys in the same shard.
pub async fn lock(latch: &Latch, key: &[u8]) -> KeyGuard {
    let sem = {
        let mut shard = latch.shards[shard_of(key)]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shard
            .entry(key.to_vec())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone()
    };
    // Wait while ANOTHER holder owns the key; the permit is handed over
    // on drop, waking exactly one waiter in FIFO order. The semaphore is
    // never closed, so the acquire error is impossible.
    let permit = sem.acquire_owned().await.expect("latch semaphore closed");
    KeyGuard { _permit: permit }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_of_is_stable_and_bounded() {
        assert_eq!(shard_of(b""), shard_of(b""));
        assert_eq!(shard_of(b"70/k"), shard_of(b"70/k"));
        assert_ne!(shard_of(b"70/k"), shard_of(b"70/j"));
        for k in ["a", "70/\x02\x00\x00\x00\x01k", "long root key bytes"] {
            assert!(shard_of(k.as_bytes()) < SHARDS);
        }
    }

    #[tokio::test]
    async fn same_key_serializes_different_keys_do_not() {
        let latch = Latch::new();
        {
            let _g1 = lock(&latch, b"70/k").await;
            {
                let _g2 = lock(&latch, b"71/other").await;
                // different roots: both held simultaneously without deadlock
            }
            // re-locking the SAME key here would self-deadlock; asserted
            // across tasks below instead.
        }
        // dropped guards release: a fresh lock succeeds immediately.
        let _g3 = lock(&latch, b"70/k").await;
    }

    /// A guard held ACROSS an await must not block the runtime: another
    /// task acquires a different key, then the same key once released.
    #[tokio::test]
    async fn held_across_await_does_not_stall_other_tasks() {
        let latch = Arc::new(Latch::new());
        let holder = tokio::spawn({
            let l = Arc::clone(&latch);
            async move {
                let _g = lock(&l, b"70/k").await;
                // park WITH the guard held (the commit-across-await shape)
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        // unrelated key: immediate even while 70/k is held.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let _other = lock(&latch, b"71/x").await;
        })
        .await
        .expect("different key must not wait on 70/k");
        holder.await.unwrap();
        // after release the same key is lockable again.
        tokio::time::timeout(std::time::Duration::from_secs(2), lock(&latch, b"70/k"))
            .await
            .expect("release must wake the next waiter");
    }

    #[tokio::test]
    async fn mutual_exclusion_across_tasks() {
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
            handles.push(tokio::spawn(async move {
                for _ in 0..200 {
                    let _g = lock(&l, b"70/hot").await;
                    {
                        let mut seen = i.lock().unwrap();
                        *seen += 1;
                        assert_eq!(*seen, 1, "two tasks inside the latch");
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
            h.await.unwrap();
        }
        assert_eq!(*counter.lock().unwrap(), 8 * 200);
    }
}
