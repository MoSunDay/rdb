//! M3: cluster-global timestamp allocation (the `ClusterTs` core).
//!
//! M1's oracle hands out node-local timestamps; once `cluster init`
//! makes `topology.cluster_ready` true, every node switches (behind the
//! SAME [`super::Oracle`] API) to blocks leased from a raft-authorized
//! global cursor:
//! - the leader persists `sql_ts_cursor = lo + size` via raft BEFORE a
//!   block `[lo, lo+size)` is served, so a crash or leadership change
//!   never reuses a timestamp (the new leader's first `lo` is the cursor
//!   the old one committed; gaps are fine, only reuse is not);
//! - followers lease blocks over the control API (`/sql/ts`), resolving
//!   the leader through the `sql_nodes` registry (leader raft addr ->
//!   http bind) refreshed from the FSM kv dump every ~3s;
//! - `alloc`/`alloc_n` NEVER block on raft or HTTP: a background
//!   refiller keeps blocks stocked. When the local block is exhausted
//!   before a refill lands, the node serves a LOCALLY-bumped range
//!   above `last_cursor + TS_FALLBACK_GAP` (and `global_hi`), marks
//!   itself degraded and keeps retrying; the next successful fetch
//!   pushes the cursor above its floor (`floor = global_hi + 1`),
//!   re-anchoring the global sequence above every fallback grant.
//!
//! `now()` is the node's LOCAL knowledge (`global_hi`): it may lag
//! cluster-wide grants by up to one block + one refresh, which is safe
//! for snapshot reads (an older read timestamp still observes a
//! consistent committed prefix).

use std::ops::Range;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::rcache::join;
use crate::rtypes::RaftLogEntryData;
use crate::state::{self, RaftState};
use crate::topology::Topology;

use super::nodes::{leader_http_addr, NodeBinds};

#[path = "global_tests.rs"]
#[cfg(test)]
mod global_tests;

/// FSM key holding the exclusive end of all raft-authorized blocks (the
/// next grantable timestamp; "cursor-before-serve" writes it ahead of
/// any local carving).
pub const TS_CURSOR_KEY: &str = "sql_ts_cursor";
/// Default block size leased per fetch (bounded so the persisted cursor
/// never runs far ahead of the served timestamps).
pub const TS_BLOCK: u64 = 4096;
/// Headroom the degraded fallback jumps above the last observed cursor:
/// every block granted elsewhere is a sub-range below the cursor, so a
/// fallback starting `GAP` above it cannot collide until the cluster
/// has allocated through the gap -- at a pathological 1M ts/s that is
/// 4+ seconds, versus a 200ms refill retry that re-anchors the cursor
/// above the degraded node's floor long before.
pub const TS_FALLBACK_GAP: u64 = 4 << 20;
/// Upper bound on one remote lease (`/sql/ts?n=`); bigger `alloc_n`s
/// are served by the requester's own (unbounded) local fallback.
pub const MAX_REMOTE_BLOCK: u64 = 1 << 20;

/// Refill cadence; also how fast a degraded node recovers its floor.
const REFILL_INTERVAL: Duration = Duration::from_millis(200);
/// Fetch the next block once the current one is half drained.
const REFILL_LOW_WATER: u64 = TS_BLOCK / 2;

/// Narrow, cloneable dependencies injected into [`ClusterTs`] (never
/// the whole `Shared` -- that would create Arc cycles with
/// `Shared.sql_ts`).
#[derive(Clone)]
pub struct ClusterTsDeps {
    pub raft: Arc<RwLock<RaftState>>,
    pub topo: Arc<RwLock<Topology>>,
    pub binds: NodeBinds,
    pub token: String,
}

/// Mutex-guarded allocation state; no await ever holds this lock (raft
/// applies and HTTP fetches run outside, under `fetch_mux`).
#[derive(Default)]
pub struct TsState {
    /// Locally servable block `[block_lo, block_hi)` (empty when equal).
    pub block_lo: u64,
    pub block_hi: u64,
    /// Highest timestamp this node knows has been granted (carved,
    /// fetched or observed); returned by `now()`.
    pub global_hi: u64,
    /// Last raft cursor observed (exclusive end of every block
    /// authorized so far); the fallback jumps above `cursor + GAP`.
    pub last_cursor: u64,
    /// True while allocations come from the locally-bumped fallback.
    pub degraded: bool,
}

pub struct ClusterTs {
    deps: ClusterTsDeps,
    state: Mutex<TsState>,
    /// Serializes block fetches (refiller vs `/sql/ts` handler): two
    /// concurrent fetches would compute the same `lo` from the cursor.
    fetch_mux: tokio::sync::Mutex<()>,
}

// ---- pure helpers (unit-tested in global_tests.rs) ----

pub fn remaining(st: &TsState) -> u64 {
    st.block_hi.saturating_sub(st.block_lo)
}

/// Carve `n` consecutive timestamps from the local block; `None` when
/// the block is exhausted or smaller than `n`.
pub fn carve(st: &mut TsState, n: u64) -> Option<Range<u64>> {
    if n == 0 {
        return Some(st.block_lo..st.block_lo);
    }
    if remaining(st) < n {
        return None;
    }
    let r = st.block_lo..st.block_lo + n;
    st.block_lo = r.end;
    st.global_hi = st.global_hi.max(r.end - 1);
    Some(r)
}

/// Like [`carve`] but only when the WHOLE local block sits at or above
/// `floor` (the requester already granted everything below it).
pub fn carve_above_floor(st: &mut TsState, n: u64, floor: u64) -> Option<Range<u64>> {
    (st.block_lo >= floor).then(|| carve(st, n)).flatten()
}

/// Degraded fallback: a locally-bumped range above every grant this node
/// knows of. Correct (monotonic, never reused locally, disjoint from
/// all cursor-authorized blocks for at least `TS_FALLBACK_GAP`);
/// recovered by the next fetch honoring `floor = global_hi + 1`.
pub fn fallback_range(st: &mut TsState, n: u64) -> Range<u64> {
    let lo = st.global_hi.max(st.last_cursor + TS_FALLBACK_GAP) + 1;
    let r = lo..lo + n;
    st.global_hi = st.global_hi.max(r.end.saturating_sub(1));
    st.degraded = true;
    r
}

/// Adopt a fetched block; rejected (false) unless it sits strictly above
/// everything already granted (`lo > global_hi`) -- a misbehaving or
/// stale leader must never walk the sequence backwards.
pub fn install_block(st: &mut TsState, lo: u64, hi: u64) -> bool {
    if hi <= lo || lo <= st.global_hi {
        return false;
    }
    st.block_lo = lo;
    st.block_hi = hi;
    st.global_hi = st.global_hi.max(hi - 1);
    st.last_cursor = st.last_cursor.max(hi);
    true
}

/// First `lo` of a new block: never 0 (the oracle's local sequence
/// starts at 1), never below the raft cursor (crash-safety) and never
/// below the requester's floor (degraded recovery).
pub fn next_block_lo(cursor: u64, floor: u64) -> u64 {
    cursor.max(floor).max(1)
}

/// Parse a `/sql/ts` body ("lo hi") into a non-empty block.
pub fn parse_ts_block(body: &str) -> Option<(u64, u64)> {
    let mut it = body.split_whitespace();
    let lo = it.next()?.parse::<u64>().ok()?;
    let hi = it.next()?.parse::<u64>().ok()?;
    (hi > lo).then_some((lo, hi))
}

impl ClusterTs {
    pub fn new(deps: ClusterTsDeps) -> ClusterTs {
        ClusterTs {
            deps,
            state: Mutex::new(TsState::default()),
            fetch_mux: tokio::sync::Mutex::new(()),
        }
    }

    /// Narrow dependency view (registration loop and callers re-use it).
    pub fn deps(&self) -> &ClusterTsDeps {
        &self.deps
    }

    /// Cluster mode is only active once `cluster init` made the topology
    /// ready; otherwise the oracle keeps its exact local-atomic
    /// behavior (and mirrors local grants into `global_hi` so the later
    /// switch can never reuse a pre-cluster timestamp).
    pub fn active(&self) -> bool {
        self.deps.topo.read().unwrap().cluster_ready
    }

    /// Allocate `n` timestamps; sync, never blocks on raft/HTTP.
    pub fn alloc_n(&self, n: u64) -> Range<u64> {
        let mut st = self.state.lock().unwrap();
        match carve(&mut st, n) {
            Some(r) => r,
            None => {
                if !st.degraded {
                    eprintln!(
                        "sql ts: no block left (above {}, gap {}), degraded until refill",
                        st.global_hi, TS_FALLBACK_GAP
                    );
                }
                fallback_range(&mut st, n)
            }
        }
    }

    /// Local knowledge of the newest granted timestamp (`global_hi`).
    pub fn now(&self) -> u64 {
        self.state.lock().unwrap().global_hi
    }

    /// Fold an externally granted high-water mark into `global_hi`
    /// (local-atomic allocations made while the cluster core is
    /// installed but not yet active).
    pub fn observe_floor(&self, hi: u64) {
        let mut st = self.state.lock().unwrap();
        st.global_hi = st.global_hi.max(hi);
    }

    /// True once a fetched block has room (tests + refill polls).
    pub fn has_block(&self) -> bool {
        remaining(&self.state.lock().unwrap()) > 0
    }

    pub fn degraded(&self) -> bool {
        self.state.lock().unwrap().degraded
    }

    /// One refill round: fetch a fresh block when the local one is low.
    pub async fn refill_once(&self) -> Result<(), String> {
        if !self.active() {
            return Ok(());
        }
        let (need, floor) = {
            let st = self.state.lock().unwrap();
            (remaining(&st) < REFILL_LOW_WATER, st.global_hi + 1)
        };
        if !need {
            let mut st = self.state.lock().unwrap();
            if st.degraded && remaining(&st) >= REFILL_LOW_WATER {
                st.degraded = false;
            }
            return Ok(());
        }
        let (lo, hi) = self.fetch_serialized(TS_BLOCK, floor).await?;
        let mut st = self.state.lock().unwrap();
        if !install_block(&mut st, lo, hi) {
            return Err(format!(
                "rejected block {lo}..{hi} at global_hi {}",
                st.global_hi
            ));
        }
        if st.degraded {
            st.degraded = false;
            eprintln!("sql ts: cluster blocks recovered, serving [{lo},{hi})");
        }
        Ok(())
    }

    /// Lease `n` timestamps for a REMOTE follower (`/sql/ts`): carve
    /// from the local block when it fits above `floor`, else fetch
    /// (raft cursor write) first. Same carve logic everywhere.
    pub async fn carve_remote(&self, n: u64, floor: u64) -> Result<Range<u64>, String> {
        let fits = {
            let mut st = self.state.lock().unwrap(); // dropped at block end
            carve_above_floor(&mut st, n, floor)
        };
        if let Some(r) = fits {
            return Ok(r);
        }
        let (lo, hi) = self.fetch_serialized(n.max(TS_BLOCK), floor).await?;
        let mut st = self.state.lock().unwrap();
        if !install_block(&mut st, lo, hi) {
            return Err(format!(
                "rejected block {lo}..{hi} at global_hi {}",
                st.global_hi
            ));
        }
        st.degraded = false;
        carve(&mut st, n).ok_or_else(|| "fresh block smaller than n".to_string())
    }

    /// One fetch under `fetch_mux` (refiller and `/sql/ts` share it so
    /// two fetches can never compute the same `lo` from one cursor).
    async fn fetch_serialized(&self, want: u64, floor: u64) -> Result<(u64, u64), String> {
        let _fetch = self.fetch_mux.lock().await;
        let is_leader = self.deps.raft.read().unwrap().is_leader;
        if is_leader {
            leader_fetch(&self.deps, want, floor).await
        } else {
            follower_fetch(&self.deps, want, floor).await
        }
    }
}

/// Leader block fetch. `sql_ts_cursor` is persisted via the standard
/// apply path BEFORE the range is returned; the stub path (no apply
/// channel) makes this unit-testable with a plain `RaftState`.
pub async fn leader_fetch(
    deps: &ClusterTsDeps,
    want: u64,
    floor: u64,
) -> Result<(u64, u64), String> {
    let cursor = {
        let r = deps.raft.read().unwrap();
        state::raft_get(&r, TS_CURSOR_KEY)
            .parse::<u64>()
            .unwrap_or(0)
    };
    let lo = next_block_lo(cursor, floor);
    let size = want.clamp(TS_BLOCK, MAX_REMOTE_BLOCK.max(TS_BLOCK));
    let entry = RaftLogEntryData {
        key: TS_CURSOR_KEY.to_string(),
        value: (lo + size).to_string(),
    };
    let ticket = {
        let mut r = deps.raft.write().unwrap();
        state::raft_apply_start(&mut r, &entry)?
    };
    state::raft_apply_await(ticket).await?;
    Ok((lo, lo + size))
}

/// Follower block fetch over the control API. The leader's http bind is
/// resolved from the `sql_nodes` registry via `RaftState.leader_addr`.
pub async fn follower_fetch(
    deps: &ClusterTsDeps,
    want: u64,
    floor: u64,
) -> Result<(u64, u64), String> {
    let http = leader_http_addr(&deps.raft).ok_or("leader http addr unknown (sql_nodes)")?;
    let url = format!(
        "http://{http}/sql/ts?n={want}&floor={floor}&raft-token={}",
        join::percent_encode(&deps.token)
    );
    let (status, body) = join::http_get_status(&url).await?;
    if status != 200 {
        return Err(format!("leader /sql/ts status {status}: {body}"));
    }
    parse_ts_block(&body).ok_or_else(|| format!("bad /sql/ts body: {body}"))
}

/// `/sql/ts?n=&floor=&raft-token=` route: leader-only carve (the same
/// logic the leader's own allocations use), auth via the raft token like
/// every control-API route. Served only when a cluster core is installed.
pub async fn route_sql_ts(
    ts: Option<&Arc<ClusterTs>>,
    token: &str,
    params: &[(String, String)],
) -> (&'static str, String) {
    let Some(ts) = ts else {
        return ("404 Not Found", "404 page not found\n".to_string());
    };
    if crate::rcache::http::first_param(params, "raft-token") != token {
        return ("401 Unauthorized", "unauthorized\n".to_string());
    }
    if !ts.deps.raft.read().unwrap().is_leader {
        return ("404 Not Found", "not leader\n".to_string());
    }
    let n = param_u64(params, "n")
        .unwrap_or(1)
        .clamp(1, MAX_REMOTE_BLOCK);
    let floor = param_u64(params, "floor").unwrap_or(0);
    match ts.carve_remote(n, floor).await {
        Ok(r) => ("200 OK", format!("{} {}\n", r.start, r.end)),
        Err(e) => ("200 OK", format!("internal error: {e}\n")),
    }
}

fn param_u64(params: &[(String, String)], key: &str) -> Option<u64> {
    crate::rcache::http::first_param(params, key)
        .parse::<u64>()
        .ok()
}

/// Background refiller: keeps a healthy block stocked; logs each failure
/// EPISODE once until the next recovery.
pub fn spawn_refill(ts: Arc<ClusterTs>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFILL_INTERVAL);
        ticker.tick().await; // consume the immediate first tick
        let mut logged = false;
        loop {
            ticker.tick().await;
            match ts.refill_once().await {
                Ok(()) => logged = false,
                Err(e) => {
                    if !logged {
                        eprintln!("sql ts: block refill failed: {e} (degraded fallback active)");
                        logged = true;
                    }
                }
            }
        }
    });
}
