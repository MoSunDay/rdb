//! Timestamp oracle for the MVCC version chains (M1: node-local atomic).
//!
//! Every SQL write batch stamps its row versions with a timestamp from
//! this oracle; readers pick, per primary key, the newest version with
//! `commit_ts <= read_ts` (snapshot reads). M1 hands out timestamps from
//! one node-local atomic counter -- correct for single-node clusters and
//! for per-node local-slot queries; M3 swaps the allocation for a
//! raft-authorized global oracle behind the same interface.
//!
//! The oracle also tracks the set of live snapshot timestamps (M2:
//! explicit BEGIN..COMMIT sessions) so version GC knows the oldest
//! timestamp still readable (`watermark`).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Timestamp oracle + live-snapshot registry.
pub struct Oracle {
    next: AtomicU64,
    snaps: Mutex<BTreeSet<u64>>,
}

impl Default for Oracle {
    fn default() -> Self {
        Self::new()
    }
}

impl Oracle {
    pub fn new() -> Oracle {
        Oracle {
            next: AtomicU64::new(1),
            snaps: Mutex::new(BTreeSet::new()),
        }
    }

    /// Allocate one fresh timestamp.
    pub fn alloc(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }

    /// Reserve `n` consecutive timestamps at once (one write batch stamps
    /// many versions); the range is `[start, start+n)`.
    pub fn alloc_n(&self, n: u64) -> std::ops::Range<u64> {
        let start = self.next.fetch_add(n, Ordering::SeqCst);
        start..start + n
    }

    /// Latest timestamp handed out (a safe read point for new snapshots:
    /// every version with `ts <= now()` is committed).
    pub fn now(&self) -> u64 {
        self.next.load(Ordering::SeqCst) - 1
    }

    /// Register an open snapshot (M2 BEGIN); idempotent per ts.
    pub fn register_snapshot(&self, ts: u64) {
        self.snaps.lock().unwrap().insert(ts);
    }

    /// Drop a finished snapshot (COMMIT/ROLLBACK/connection end).
    pub fn unregister_snapshot(&self, ts: u64) {
        self.snaps.lock().unwrap().remove(&ts);
    }

    /// Oldest still-registered snapshot, or the current `now()` when no
    /// snapshot is open: versions strictly older than the newest version
    /// at or below the watermark are garbage-collectable.
    pub fn watermark(&self) -> u64 {
        let snaps = self.snaps.lock().unwrap();
        snaps.iter().next().copied().unwrap_or_else(|| self.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_is_monotonic_and_dense() {
        let o = Oracle::new();
        assert_eq!(o.alloc(), 1);
        assert_eq!(o.alloc(), 2);
        let r = o.alloc_n(3);
        assert_eq!(r, 3..6);
        assert_eq!(o.alloc(), 6);
        assert_eq!(o.now(), 6);
    }

    #[test]
    fn watermark_tracks_registered_snapshots() {
        let o = Oracle::new();
        o.alloc_n(10);
        assert_eq!(o.watermark(), 10);
        o.register_snapshot(4);
        o.register_snapshot(7);
        assert_eq!(o.watermark(), 4);
        o.unregister_snapshot(4);
        assert_eq!(o.watermark(), 7);
        o.unregister_snapshot(7);
        assert_eq!(o.watermark(), 10);
    }
}
