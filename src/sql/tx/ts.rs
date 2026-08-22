//! Timestamp oracle for the MVCC version chains.
//!
//! Every SQL write batch stamps its row versions with a timestamp from
//! this oracle; readers pick, per primary key, the newest version with
//! `commit_ts <= read_ts` (snapshot reads). M1 hands out timestamps
//! from one node-local atomic counter; M3 swaps the allocation for
//! raft-authorized cluster-global blocks behind the same interface (see
//! [`global`]): once `enable_cluster` has installed a [`ClusterTs`]
//! core AND the topology reports `cluster_ready`, `alloc/alloc_n/now`
//! delegate to it; until then (and on every non-cluster node) the
//! exact M1 local-atomic behavior is kept. Mode switches are monotonic
//! in both directions: local grants are mirrored into the cluster's
//! `global_hi` and cluster grants bump the local counter.
//!
//! The oracle also tracks the set of live snapshot timestamps (M2:
//! explicit BEGIN..COMMIT sessions) so version GC knows the oldest
//! timestamp still readable (`watermark`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::global::ClusterTs;

/// Timestamp oracle + live-snapshot registry.
pub struct Oracle {
    next: AtomicU64,
    /// ts -> number of open snapshots at that ts (two BEGINs without an
    /// intervening write share one ts; refcounting keeps the watermark
    /// pinned until the LAST holder finishes).
    snaps: Mutex<BTreeMap<u64, u32>>,
    /// M3 cluster core, installed once by main after raft + topology
    /// exist (composition, no Arc cycles: `ClusterTs` holds narrow deps,
    /// never the `Shared`).
    cluster: OnceLock<Arc<ClusterTs>>,
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
            snaps: Mutex::new(BTreeMap::new()),
            cluster: OnceLock::new(),
        }
    }

    /// Install the M3 cluster core (first install wins; later calls are
    /// no-ops). Seeds the cluster's floor with the local counter (and
    /// the local counter with the cluster's) so the local->cluster and
    /// cluster->local switches can never reuse a timestamp.
    pub fn enable_cluster(&self, ts: Arc<ClusterTs>) -> bool {
        ts.observe_floor(self.now());
        let installed = self.cluster.set(ts).is_ok();
        if let Some(c) = self.cluster.get() {
            self.next.fetch_max(c.now() + 1, Ordering::SeqCst);
        }
        installed
    }

    /// The installed cluster core when cluster mode is ACTIVE
    /// (`topology.cluster_ready`); `None` keeps local-atomic behavior.
    fn cluster_core(&self) -> Option<&Arc<ClusterTs>> {
        let core = self.cluster.get()?;
        core.active().then_some(core)
    }

    /// Allocate one fresh timestamp.
    pub fn alloc(&self) -> u64 {
        self.alloc_n(1).start
    }

    /// Reserve `n` consecutive timestamps at once (one write batch stamps
    /// many versions); the range is `[start, start+n)`.
    pub fn alloc_n(&self, n: u64) -> std::ops::Range<u64> {
        match self.cluster.get() {
            Some(core) if core.active() => {
                let r = core.alloc_n(n);
                // Keep the local counter above every cluster grant so a
                // later cluster->local switch stays monotonic.
                self.next.fetch_max(r.end, Ordering::SeqCst);
                r
            }
            Some(core) => {
                // Core installed but cluster not ready yet: local grants,
                // mirrored into the cluster floor for the same reason.
                let start = self.next.fetch_add(n, Ordering::SeqCst);
                core.observe_floor(start + n.max(1) - 1);
                start..start + n
            }
            None => {
                let start = self.next.fetch_add(n, Ordering::SeqCst);
                start..start + n
            }
        }
    }

    /// Latest timestamp handed out (a safe read point for new snapshots:
    /// every version with `ts <= now()` is committed). In cluster mode
    /// this is the node's LOCAL knowledge of the global sequence
    /// (`global_hi`); it may lag cluster-wide grants by up to one block
    /// + one refresh, which is safe for snapshot reads.
    pub fn now(&self) -> u64 {
        match self.cluster_core() {
            Some(core) => core.now(),
            None => self.next.load(Ordering::SeqCst) - 1,
        }
    }

    /// Raise this node's knowledge of the global sequence past `ts`
    /// (a 2PC participant applying a commit decided elsewhere: without
    /// this, a node that never allocated timestamps itself would hand
    /// out snapshots below the commit ts and never see the rows it
    /// just flipped visible).
    pub fn advance_to(&self, ts: u64) {
        if let Some(core) = self.cluster.get() {
            core.observe_floor(ts);
        }
        self.next.fetch_max(ts.saturating_add(1), Ordering::SeqCst);
    }

    /// Register an open snapshot (M2 BEGIN); refcounted per ts.
    pub fn register_snapshot(&self, ts: u64) {
        *self.snaps.lock().unwrap().entry(ts).or_insert(0) += 1;
    }

    /// Drop one finished snapshot holder (COMMIT/ROLLBACK/connection
    /// end); the ts leaves the registry only when the last holder is
    /// gone. Unregistering an unregistered ts is a no-op.
    pub fn unregister_snapshot(&self, ts: u64) {
        let mut snaps = self.snaps.lock().unwrap();
        if let Some(count) = snaps.get_mut(&ts) {
            *count -= 1;
            if *count == 0 {
                snaps.remove(&ts);
            }
        }
    }

    /// Oldest still-registered snapshot, or the current `now()` when no
    /// snapshot is open: versions strictly older than the newest version
    /// at or below the watermark are garbage-collectable.
    pub fn watermark(&self) -> u64 {
        let snaps = self.snaps.lock().unwrap();
        snaps.keys().next().copied().unwrap_or_else(|| self.now())
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
