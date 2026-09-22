//! Consumer-group coordinator (P3): an IN-MEMORY registry of group
//! states driven by the pure machine in `state.rs`. One `CoordRuntime`
//! per process is created by `kafka::serve`, handed to every
//! connection task (`conn::handle_conn`), and swept by the background
//! task in `session.rs` (member expiry / rebalance deadlines).
//!
//! Persistence contract: membership is coordinator-local -- a restart
//! drops every group (DescribeGroups then answers "Dead"), and clients
//! rejoin natively (JoinGroup re-creates the group). Committed offsets
//! live in the kind 0x20 ledger and survive restarts untouched; the
//! generation carried in ledger rows is the cross-restart fence.
//!
//! Layout: `state.rs` (pure transitions + event lists), `session.rs`
//! (wall clock, Notify registry, expiry sweep), `join.rs` (async join
//! barrier / sync handoff), `api.rs` + `group_api.rs` (wire handlers).

pub mod api;
pub mod group_api;
pub mod join;
pub mod session;
pub mod state;
#[cfg(test)]
#[path = "api_tests.rs"]
mod api_tests;
#[cfg(test)]
#[path = "group_api_tests.rs"]
mod group_api_tests;
#[cfg(test)]
#[path = "state_tests.rs"]
mod state_tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::kafka::errors;

/// Coordinator runtime state: the group table plus the per-group Notify
/// registry and the member-id counter. Plain data + free functions
/// (the `ds::wait` mold); no lifecycle methods beyond construction.
pub struct CoordRuntime {
    /// group -> live state (absent = Dead / never existed).
    pub groups: RwLock<HashMap<String, state::GroupState>>,
    /// group -> wakeup channel for parked JoinGroup/SyncGroup calls.
    pub notifies: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    member_seq: AtomicU64,
}

impl CoordRuntime {
    pub fn new() -> CoordRuntime {
        CoordRuntime {
            groups: RwLock::new(HashMap::new()),
            notifies: Mutex::new(HashMap::new()),
            // Seeded from the wall clock: member ids must not repeat
            // across restarts, or a zombie from the previous process
            // incarnation could collide with a fresh member id (same
            // string, same generation) and slip past the fence.
            member_seq: AtomicU64::new(now_seed()),
        }
    }
}

impl Default for CoordRuntime {
    fn default() -> Self {
        CoordRuntime::new()
    }
}

/// Per-process id seed: microseconds since the epoch (monotonic
/// enough across restarts; two processes on one host can still
/// collide, which only matters for multi-broker setups).
fn now_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(1)
        .max(1)
}

/// Fresh member id (`rdb-<group>-<n>`; the broker's uuid format is not
/// load-bearing -- clients treat it as an opaque string).
pub fn next_member_id(rt: &CoordRuntime, group: &str) -> String {
    let n = rt.member_seq.fetch_add(1, Ordering::Relaxed);
    format!("rdb-{}-{}", sanitize(group), n)
}

/// Keep ids ASCII-safe without '/' (group names may carry odd bytes).
fn sanitize(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' { b } else { b'_' })
        .take(32)
        .map(|b| b as char)
        .collect()
}

/// `true` when the group has live members (a fenceable stage).
fn active(st: &state::GroupState) -> bool {
    !st.members.is_empty()
}

/// Generation/member fencing for OffsetCommit v1+ (the FIRST fence
/// layer; the ledger's stored generation is the second). Returns the
/// error code to answer EVERY partition with, or `None` to proceed:
/// - group absent from the runtime (never joined, or restart wiped
///   membership): degraded mode -- ledger-only comparison downstream.
/// - group present but empty (all members gone): same degraded mode
///   (an Empty group has nobody to authorize; ledger rows keep the
///   last committed generation as high-water).
/// - group active: member must be enrolled and the generation must be
///   the CURRENT one (older -> ILLEGAL_GENERATION, unknown member ->
///   UNKNOWN_MEMBER_ID; a zombie cannot pass either).
pub fn commit_fence(rt: &CoordRuntime, group: &str, generation: i32, member_id: &str) -> Option<i16> {
    let groups = rt
        .groups
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let st = groups.get(group)?;
    if !active(st) {
        return None;
    }
    if !st.members.contains_key(member_id) {
        return Some(errors::UNKNOWN_MEMBER_ID);
    }
    if st.generation != generation {
        return Some(errors::ILLEGAL_GENERATION);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_ids_are_unique_and_safe() {
        let rt = CoordRuntime::new();
        let a = next_member_id(&rt, "g1");
        let b = next_member_id(&rt, "g1");
        assert!(a.starts_with("rdb-g1-") && b.starts_with("rdb-g1-"));
        assert_ne!(a, b, "ids are unique within a runtime");
        let c = next_member_id(&rt, "a/b c");
        assert!(c.starts_with("rdb-a_b_c-"), "odd bytes sanitized: {c}");
        // The seed is the process clock, so a restarted process hands
        // out ids the old one could never have issued.
        let seed = rt.member_seq.load(Ordering::Relaxed);
        assert!(seed > 1_000_000_000_000, "seeded from the wall clock: {seed}");
    }

    #[test]
    fn fence_layers() {
        let rt = CoordRuntime::new();
        // Absent group: degraded mode (None = ledger decides).
        assert_eq!(commit_fence(&rt, "g", 1, "m"), None);
        // Active group: full fencing.
        let mut st = state::new_group("consumer");
        let a = state::JoinArgs {
            member_id: "m1",
            instance_id: None,
            client_id: "c",
            client_host: "h",
            session_timeout_ms: 1000,
            rebalance_timeout_ms: 4000,
            protocol_type: "consumer",
            protocol_name: "range",
            metadata: b"",
            now_ms: 0,
        };
        let (st2, _, _) = state::join(st.clone(), &a);
        st = st2;
        rt.groups.write().unwrap().insert("g".into(), st);
        assert_eq!(commit_fence(&rt, "g", 1, "m1"), None, "enrolled, current gen");
        assert_eq!(commit_fence(&rt, "g", 0, "m1"), Some(errors::ILLEGAL_GENERATION));
        assert_eq!(commit_fence(&rt, "g", 1, "ghost"), Some(errors::UNKNOWN_MEMBER_ID));
        // Empty group: degraded mode again.
        let empty = state::to_empty(state::new_group("consumer"));
        rt.groups.write().unwrap().insert("g".into(), empty);
        assert_eq!(commit_fence(&rt, "g", 99, "m1"), None);
    }
}
