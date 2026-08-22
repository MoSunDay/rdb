//! HA observer/failover (Rust port of the goroutines Go embeds in
//! `internal/server/server.go`, NOT `internal/rcache`: `handlerObserver`,
//! `updateClusterStableSlots`, the 5s leader probe loop, and the 1s
//! `backup_target_map` init loop).
//!
//! Deviations from Go (all deliberate):
//! - `Raft.VerifyLeader()` maps to openraft's `Raft::ensure_linearizable()`
//!   (successor of the deprecated `is_leader`).
//! - Go's empty-`AppendEntriesRequest` peer probe (10s timeout) becomes a
//!   plain `TcpStream::connect` liveness check with a 5s timeout (openraft
//!   exposes no raw RPC send); the RPC payload's `term` is not computed.
//! - openraft has no observer channel, so a failed probe drives the Failed
//!   direction (the probe covers both directions).
//! - Go's self-recovery indexes the backup map with no length check and
//!   panics on malformed data; a `len == 2` guard plus log replaces it.
//! - Go's backup-map init loop `Fatalf`s the whole process when an apply
//!   fails; D7a breaks the tick's entry loop instead and retries the
//!   whole (idempotent, blind-overwrite) list on the next 1s tick.
//! - Go's leader-notify goroutine only toggles the unread HTTP
//!   `ENABLE_WRITE` flag: intentionally skipped.
//! - Go's `strings.Replace`/`strings.Contains` on the instances list are
//!   SUBSTRING operations and corrupt overlapping addresses ("10.0.0.1"
//!   also matches inside "10.0.0.11"); the port replaces/matches exact
//!   comma-separated elements and rejects empty src/target map halves as
//!   BadMap.
//! - Go's `handlerObserver`/`updateClusterStableSlots` read the FSM, then
//!   blind-apply over several awaits: concurrent observers (or a lock-free
//!   `cluster init` write) decide on stale snapshots and clobber each
//!   other. The port serializes the whole read -> decide -> apply sequence
//!   on a per-process [`ObserverMux`] and CONVERGES (re-read + re-decide
//!   after every successful apply). Log order thus deviates slightly from
//!   Go: "done" is only printed after a successful apply (Go logs it even
//!   when `raft.Apply` failed), and a convergence re-check round stays
//!   silent instead of re-logging "don't need update".

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use openraft::metrics::RaftMetrics;

use crate::conf;
use crate::rcache::fsm::KvMap;
use crate::rcache::{Node, NodeId, RdbRaft};
use crate::rtypes;
use crate::state::APPLY_TIMEOUT;
use crate::topology;

/// Serializes every observer "read snapshots -> decide -> raft apply"
/// sequence on one process (probe loop, self-recovery and any concurrent
/// `handler_observer` caller share a single instance): each FSM read is
/// separated from the apply by awaits, so two unlocked in-flight
/// observers decide on stale snapshots and blind-write over each other —
/// or over a lock-free writer such as `cluster init`.
pub type ObserverMux = Arc<tokio::sync::Mutex<()>>;

/// A fresh observer mux (create once per process, share it).
pub fn observer_mux() -> ObserverMux {
    Arc::new(tokio::sync::Mutex::new(()))
}

/// Replicated routing key (Go `cluster_slots_stable_instances`).
const INSTANCES_KEY: &str = "cluster_slots_stable_instances";
/// Observer event kinds (Go retType strings, byte-for-byte).
const RET_FAILED: &str = "FailedHeartbeatObservation";
const RET_RESUMED: &str = "ResumedHeartbeatObservation";
/// Sentinel key written once the config backup map has been seeded.
const BACKUP_INIT_KEY: &str = "backup_target_map_init";

/// Go probe loop: `time.Sleep(5 * time.Second)`.
const PROBE_INTERVAL: Duration = Duration::from_secs(5);
/// Deviation: Go's raw AppendEntries probe used 10s; TCP-connect uses 5s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Go init loop: `time.Sleep(1 * time.Second)`.
const INIT_INTERVAL: Duration = Duration::from_secs(1);

/// Pure failover decision (Go `handlerObserver` minus FSM reads); richer
/// than [`failover_action`] so callers can reproduce Go's per-branch logs.
#[derive(Debug, PartialEq, Eq)]
enum FailoverDecision {
    /// Replicate this new instances value.
    Replace(String),
    /// Replacement is a no-op: Go logs "<retType> <peer> don't need update".
    Unchanged,
    /// Resumed but the src address is already present: Go returns silently.
    SkipResumed,
    /// Backup map did not split into exactly two parts: Go logs
    /// "failedNodeBackupMap error: ...".
    BadMap(Vec<String>),
    /// retType is neither Failed nor Resumed: Go returns silently.
    UnknownType,
}

/// Go `handlerObserver` core over the raw `backup_target_map_<peer>`
/// value: Failed replaces src by target, Resumed swaps back unless src exists.
fn decide(ret_type: &str, backup_map_val: &str, instances: &str) -> FailoverDecision {
    if ret_type != RET_FAILED && ret_type != RET_RESUMED {
        return FailoverDecision::UnknownType;
    }
    let parts: Vec<&str> = backup_map_val.split(',').collect();
    // Both halves must be present: an empty src/target would make the
    // replacement meaningless (Go's strings.Replace with an empty old
    // inserts between characters) — treat like any other malformed map.
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        return FailoverDecision::BadMap(parts.iter().map(|s| s.to_string()).collect());
    }
    let (src, target) = (parts[0], parts[1]);
    let new = if ret_type == RET_FAILED {
        replace_element(instances, src, target)
    } else {
        if contains_element(instances, src) {
            return FailoverDecision::SkipResumed;
        }
        replace_element(instances, target, src)
    };
    if new == instances {
        FailoverDecision::Unchanged
    } else {
        FailoverDecision::Replace(new)
    }
}

/// Exact-element replace over the comma-separated `instances` list:
/// EVERY element equal to `from` becomes `to` (duplicates included).
/// Unlike `str::replace` this is not a substring replace, so an address
/// that merely CONTAINS `from` survives untouched ("10.0.0.1" no longer
/// mangles the "10.0.0.11" element).
fn replace_element(list: &str, from: &str, to: &str) -> String {
    list.split(',')
        .map(|elem| if elem == from { to } else { elem })
        .collect::<Vec<_>>()
        .join(",")
}

/// Exact-element membership over the same comma-separated list shape
/// (`str::contains` would match inside a longer element).
fn contains_element(list: &str, elem: &str) -> bool {
    list.split(',').any(|e| e == elem)
}

/// Testable pure core: the new `cluster_slots_stable_instances` value, or
/// `None` whenever Go would not replicate (bad map/no-op/unchanged/unknown).
pub fn failover_action(ret_type: &str, backup_map_val: &str, instances: &str) -> Option<String> {
    match decide(ret_type, backup_map_val, instances) {
        FailoverDecision::Replace(new) => Some(new),
        _ => None,
    }
}

/// Raft entries Go's init loop applies per config backup map, in order:
/// `backup_target_map_<k>` ("src,target") each, then the init sentinel.
fn backup_map_entries(
    map: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<rtypes::RaftLogEntryData> {
    let entries = map.iter().map(|(peer, v)| rtypes::RaftLogEntryData {
        key: format!("backup_target_map_{peer}"),
        value: format!(
            "{},{}",
            v.get("src").map(String::as_str).unwrap_or_default(),
            v.get("target").map(String::as_str).unwrap_or_default()
        ),
    });
    entries
        .chain(std::iter::once(rtypes::RaftLogEntryData {
            key: BACKUP_INIT_KEY.to_string(),
            value: "done".to_string(),
        }))
        .collect()
}

/// Go `cacheManager.Get`: missing key -> "".
fn kv_get(kv: &KvMap, key: &str) -> String {
    kv.read().unwrap().get(key).cloned().unwrap_or_default()
}

/// Leader address from metrics (Go `Raft.Leader()`; "" while unknown).
fn leader_addr_of(m: &RaftMetrics<NodeId, Node>) -> String {
    m.current_leader
        .and_then(|id| m.membership_config.membership().get_node(&id))
        .cloned()
        .unwrap_or_default()
}

/// One raft apply bounded by Go's 5s Apply timeout.
async fn raft_apply(raft: &RdbRaft, key: &str, value: &str) -> Result<(), String> {
    let entry = rtypes::RaftLogEntryData {
        key: key.to_string(),
        value: value.to_string(),
    };
    match tokio::time::timeout(APPLY_TIMEOUT, raft.client_write(entry)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("apply timeout after {}s", APPLY_TIMEOUT.as_secs())),
    }
}

/// One convergence round of the locked observer loop: what to do with the
/// LATEST committed instances value. `already_applied` marks rounds after
/// this observer's own write landed (Go has no re-check; the port's
/// re-check stays silent instead of re-logging "don't need update").
#[derive(Debug, PartialEq, Eq)]
enum RoundAction {
    /// Replicate this new instances value, then re-check.
    Apply(String),
    /// First round found nothing to change: Go logs "<retType> <peer> don't need update".
    DontNeedUpdate,
    /// Own write is durable and nothing else changed: stop silently.
    Converged,
    /// Resumed with src present / unknown retType: Go returns silently.
    Silent,
    /// Backup map malformed: Go logs "failedNodeBackupMap error: ...".
    BadMap(Vec<String>),
}

/// Map a pure [`decide`] over the fresh snapshots to a [`RoundAction`].
fn observer_round(
    already_applied: bool,
    ret_type: &str,
    backup_val: &str,
    instances: &str,
) -> RoundAction {
    match decide(ret_type, backup_val, instances) {
        FailoverDecision::Replace(new) => RoundAction::Apply(new),
        FailoverDecision::Unchanged => {
            if already_applied {
                RoundAction::Converged
            } else {
                RoundAction::DontNeedUpdate
            }
        }
        FailoverDecision::BadMap(parts) => RoundAction::BadMap(parts),
        FailoverDecision::SkipResumed | FailoverDecision::UnknownType => RoundAction::Silent,
    }
}

/// Converge `INSTANCES_KEY` toward the observer decision (caller holds
/// [`ObserverMux`]): re-read the latest committed value, decide, apply,
/// and loop only while the decision is Replace AND the apply succeeded —
/// a lock-free writer (e.g. `cluster init`) that landed meanwhile is
/// absorbed by the next round's fresh read. An apply failure stops
/// WITHOUT the Go "done" log (deviation, see module docs).
async fn converge_instances(raft: &RdbRaft, kv: &KvMap, ret_type: &str, peer_addr: &str) {
    let mut applied = false;
    loop {
        let backup_val = kv_get(kv, &format!("backup_target_map_{peer_addr}"));
        let instances = kv_get(kv, INSTANCES_KEY);
        match observer_round(applied, ret_type, &backup_val, &instances) {
            RoundAction::Apply(new) => match raft_apply(raft, INSTANCES_KEY, &new).await {
                Ok(()) => {
                    eprintln!("{ret_type} {peer_addr} done");
                    applied = true;
                }
                Err(e) => {
                    eprintln!("raft.Apply failed:{e}");
                    return;
                }
            },
            RoundAction::DontNeedUpdate => {
                eprintln!("{ret_type} {peer_addr} don't need update");
                return;
            }
            RoundAction::BadMap(parts) => {
                eprintln!("failedNodeBackupMap error: {parts:?}");
                return;
            }
            RoundAction::Converged | RoundAction::Silent => return,
        }
    }
}

/// Go `handlerObserver`: read backup map + instances from the live FSM,
/// then fail over (or back) via raft. The whole read -> decide -> apply
/// sequence is serialized on `mux` (shared with the leader probe loop and
/// self-recovery) and converges instead of trusting the pre-await
/// snapshot.
pub async fn handler_observer(
    raft: Arc<RdbRaft>,
    kv: KvMap,
    ret_type: &str,
    peer_addr: &str,
    mux: ObserverMux,
) {
    let _observer = mux.lock().await;
    converge_instances(&raft, &kv, ret_type, peer_addr).await;
}

/// Deviation probe: TCP-connect liveness check instead of Go's raw RPC.
async fn probe_peer(addr: &str) -> Result<(), String> {
    match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "connect timeout after {}s",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

/// Go self-recovery: if this leader's own entry was failed over (backup
/// address in the instances list), swap back (`len == 2` guard: Go panics).
/// Same mux + convergence discipline as the observer: serialized, and
/// re-read after every successful apply so a concurrent lock-free writer
/// is absorbed instead of clobbered.
async fn self_recover(raft: &RdbRaft, kv: &KvMap, self_addr: &str, mux: &ObserverMux) {
    let _observer = mux.lock().await;
    let mut applied = false;
    loop {
        let backup_val = kv_get(kv, &format!("backup_target_map_{self_addr}"));
        let instances = kv_get(kv, INSTANCES_KEY);
        let parts: Vec<&str> = backup_val.split(',').collect();
        if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
            if !backup_val.is_empty() {
                eprintln!("failedNodeBackupMap error: {parts:?}");
            }
            return;
        }
        if !contains_element(&instances, parts[1]) {
            // Nothing to heal: Go returns silently (no "don't need update").
            return;
        }
        let new = replace_element(&instances, parts[1], parts[0]);
        if new == instances {
            if !applied {
                eprintln!("{RET_RESUMED} {} don't need update", parts[0]);
            }
            return;
        }
        if let Err(e) = raft_apply(raft, INSTANCES_KEY, &new).await {
            eprintln!("raft.Apply failed:{e}");
            return; // no "done": the write did not land (Go deviation)
        }
        eprintln!("{RET_RESUMED} {} done", parts[0]);
        applied = true;
    }
}

/// Go leader probe: every 5s, while ready and leading, verify leadership,
/// heal the own failover entry, probe voters (success->Resumed, fail->Failed).
/// Every read-decide-apply sequence runs under `mux` (see [`ObserverMux`]).
pub fn spawn_leader_probe(
    raft: Arc<RdbRaft>,
    kv: KvMap,
    topo: Arc<RwLock<topology::Topology>>,
    self_addr: String,
    mux: ObserverMux,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PROBE_INTERVAL);
        ticker.tick().await; // Go sleeps 5s before the first iteration
        loop {
            ticker.tick().await;
            if !topo.read().unwrap().cluster_ready {
                continue;
            }
            if leader_addr_of(&raft.metrics().borrow()) != self_addr {
                continue;
            }
            // Go Raft.VerifyLeader(): confirm leadership before acting.
            if let Err(e) = raft.ensure_linearizable().await {
                eprintln!("Raft.VerifyLeader() err:{e}");
                continue;
            }
            self_recover(&raft, &kv, &self_addr, &mux).await;

            let peers: Vec<String> = {
                let m = raft.metrics().borrow().clone();
                let membership = m.membership_config.membership();
                m.membership_config
                    .voter_ids()
                    .filter(|id| *id != m.id)
                    .filter_map(|id| membership.get_node(&id).cloned())
                    .collect()
            };
            for peer in peers {
                match probe_peer(&peer).await {
                    Ok(()) => {
                        handler_observer(raft.clone(), kv.clone(), RET_RESUMED, &peer, mux.clone())
                            .await;
                    }
                    Err(e) => {
                        eprintln!("rcache heartbeat failed err:{e}");
                        handler_observer(raft.clone(), kv.clone(), RET_FAILED, &peer, mux.clone())
                            .await;
                    }
                }
            }
        }
        // Unreachable today (loop never breaks); proves exit if it ever does.
        #[allow(unreachable_code)]
        {
            eprintln!("[task-exit] ha_leader_probe (loop end)");
        }
    });
}

/// Go backup_target_map init: every 1s, while the sentinel is missing the
/// leader seeds `backup_target_map_<k>` + sentinel. Deviation (D7a): Go
/// `Fatalf`-kills the process on an apply error; here a failure only
/// breaks this tick's entry loop and the next 1s tick retries from
/// scratch — safe because the FSM apply of these keys is a blind
/// key/value overwrite (fsm.rs: `kv.insert(key, value)`), so re-applying
/// entries that already landed is idempotent, and the sentinel is applied
/// LAST so a partial pass never marks init done.
pub fn spawn_backup_map_init(raft: Arc<RdbRaft>, kv: KvMap, conf: &conf::Config) {
    // Nothing to seed: Go would loop forever without applying anything.
    if conf.backup_target_map.is_empty() {
        return;
    }
    let entries = backup_map_entries(&conf.backup_target_map);
    let self_addr = conf.raft_tcp_address.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INIT_INTERVAL);
        ticker.tick().await; // Go sleeps 1s before the first iteration
        loop {
            ticker.tick().await; // Go order: read sentinel up front,
                                 // exit on the NEXT tick after applying.
            let init = kv_get(&kv, BACKUP_INIT_KEY);
            if !init.is_empty() {
                eprintln!("init backup_target_map done.");
                eprintln!("[task-exit] ha_backup_map_init (init done)");
                return;
            }
            if leader_addr_of(&raft.metrics().borrow()) != self_addr {
                continue;
            }
            for entry in &entries {
                if let Err(e) = raft_apply(&raft, &entry.key, &entry.value).await {
                    // D7a: transient apply failures (lost leadership, 5s
                    // timeout) must not kill the process: retry the whole
                    // list on the next tick. Partially applied entries are
                    // blindly overwritten again (idempotent), and the
                    // sentinel stays unwritten until the LAST entry lands.
                    eprintln!("raft.Apply backup_target_map failed:{e} (retry next tick)");
                    break;
                }
            }
        }
        // Unreachable today (loop only exits via `return`); proves exit if it does.
        #[allow(unreachable_code)]
        {
            eprintln!("[task-exit] ha_backup_map_init (loop end)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_replaces_src_with_target() {
        let map = "127.0.0.1:32681,127.0.0.1:32684";
        let got = failover_action(RET_FAILED, map, "127.0.0.1:32681,127.0.0.1:32683");
        assert_eq!(got, Some("127.0.0.1:32684,127.0.0.1:32683".to_string()));
    }

    #[test]
    fn resumed_swaps_target_back_to_src() {
        assert_eq!(
            failover_action(RET_RESUMED, "src,tgt", "tgt,other"),
            Some("src,other".to_string())
        );
    }

    #[test]
    fn resumed_is_noop_when_src_present() {
        assert_eq!(failover_action(RET_RESUMED, "src,tgt", "src,tgt"), None);
        assert_eq!(failover_action(RET_RESUMED, "src,tgt", "src,other"), None);
    }

    #[test]
    fn malformed_backup_map_yields_none() {
        assert_eq!(failover_action(RET_FAILED, "", "a,b"), None);
        assert_eq!(failover_action(RET_FAILED, "only", "a,b"), None);
        assert_eq!(failover_action(RET_FAILED, "a,b,c", "a,b"), None);
    }

    #[test]
    fn unchanged_result_yields_none() {
        // Absent src/target: element-wise replace leaves input unchanged.
        assert_eq!(failover_action(RET_FAILED, "x,y", "a,b"), None);
        assert_eq!(failover_action(RET_RESUMED, "x,y", "a,b"), None);
    }

    #[test]
    fn failed_replaces_only_the_exact_element_not_prefix_overlaps() {
        // src "10.0.0.1" is a strict prefix of "10.0.0.11": only the exact
        // element swaps (Go's substring Replace turned this into
        // "10.0.0.2,10.0.0.21", corrupting the survivor's address).
        let got = failover_action(RET_FAILED, "10.0.0.1,10.0.0.2", "10.0.0.1,10.0.0.11");
        assert_eq!(got, Some("10.0.0.2,10.0.0.11".to_string()));
        // Duplicate elements: every equal element is replaced.
        assert_eq!(
            failover_action(RET_FAILED, "a,b", "a,a,c"),
            Some("b,b,c".to_string())
        );
        // Same for the Resumed direction (target back to src).
        assert_eq!(
            failover_action(RET_RESUMED, "10.0.0.1,10.0.0.2", "10.0.0.2,10.0.0.21"),
            Some("10.0.0.1,10.0.0.21".to_string())
        );
        // Resumed skip check is element-exact too: "ab" contains "a" as a
        // substring but is NOT the src element, so the swap proceeds.
        assert_eq!(
            failover_action(RET_RESUMED, "a,b", "ab,b"),
            Some("ab,a".to_string())
        );
    }

    #[test]
    fn empty_map_halves_are_bad_map() {
        // An empty src or target half can't name an instance: BadMap
        // (never replicated), like any other malformed value.
        assert_eq!(
            decide(RET_FAILED, ",b", "a,b"),
            FailoverDecision::BadMap(vec![String::new(), "b".to_string()])
        );
        assert_eq!(
            decide(RET_FAILED, "a,", "a,b"),
            FailoverDecision::BadMap(vec!["a".to_string(), String::new()])
        );
        assert_eq!(failover_action(RET_FAILED, ",b", "a,b"), None);
        assert_eq!(failover_action(RET_RESUMED, "a,", "a,b"), None);
    }

    #[test]
    fn unknown_ret_type_yields_none() {
        assert_eq!(failover_action("unknown", "a,b", "a,b"), None);
    }

    #[test]
    fn decisions_cover_go_log_branches() {
        assert_eq!(
            decide(RET_FAILED, "a,b", "a,c"),
            FailoverDecision::Replace("b,c".to_string())
        );
        assert_eq!(decide(RET_FAILED, "x,y", "a"), FailoverDecision::Unchanged);
        assert_eq!(
            decide(RET_RESUMED, "a,t", "a"),
            FailoverDecision::SkipResumed
        );
        assert_eq!(
            decide(RET_FAILED, "solo", "a"),
            FailoverDecision::BadMap(vec!["solo".to_string()])
        );
        assert_eq!(decide("other", "a,b", "a"), FailoverDecision::UnknownType);
    }

    #[test]
    fn backup_map_entries_match_go_apply_sequence() {
        let inner = BTreeMap::from([
            ("src".to_string(), "127.0.0.1:32681".to_string()),
            ("target".to_string(), "127.0.0.1:32684".to_string()),
        ]);
        let map = BTreeMap::from([("127.0.0.1:22681".to_string(), inner)]);
        let entries = backup_map_entries(&map);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "backup_target_map_127.0.0.1:22681");
        assert_eq!(entries[0].value, "127.0.0.1:32681,127.0.0.1:32684");
        assert_eq!(entries[1].key, BACKUP_INIT_KEY);
        assert_eq!(entries[1].value, "done");
    }

    /// D7a: a failed seed retries the WHOLE entry list on the next 1s
    /// tick. That is safe only because the retry is a no-op for entries
    /// that already landed: entries derive solely from the static config
    /// (identical every tick) and the FSM apply is a blind key/value
    /// overwrite, so partial progress never corrupts a later pass. The
    /// sentinel stays LAST: a partial pass never reads as "init done".
    #[test]
    fn backup_map_seed_retry_is_idempotent() {
        let inner = BTreeMap::from([
            ("src".to_string(), "a".to_string()),
            ("target".to_string(), "b".to_string()),
        ]);
        let map = BTreeMap::from([("p1".to_string(), inner.clone()), ("p2".to_string(), inner)]);
        let first = backup_map_entries(&map);
        let retry = backup_map_entries(&map);
        // Same list, same order, sentinel last -- on every tick.
        assert_eq!(first, retry);
        assert_eq!(first.len(), 3);
        assert_eq!(
            first[0],
            (rtypes::RaftLogEntryData {
                key: "backup_target_map_p1".to_string(),
                value: "a,b".to_string(),
            })
        );
        assert_eq!(first.last().map(|e| e.key.as_str()), Some(BACKUP_INIT_KEY));
    }

    /// The convergence loop's round mapping: a lock-free writer that lands
    /// between this observer's own apply and the re-check is absorbed —
    /// the next round decides on the WRITER's value, folds this observer's
    /// effect into it, and only stops once nothing changes anymore.
    #[test]
    fn converge_rounds_absorb_lock_free_writer() {
        // Round 1 (fresh): fail over a -> a2 over "a,b,c".
        assert_eq!(
            observer_round(false, RET_FAILED, "a,a2", "a,b,c"),
            RoundAction::Apply("a2,b,c".to_string())
        );
        // Round 2: an operator wrote "a,b,c,d" over our "a2,b,c" while we
        // held the lock — decide again on the fresh value (union).
        assert_eq!(
            observer_round(true, RET_FAILED, "a,a2", "a,b,c,d"),
            RoundAction::Apply("a2,b,c,d".to_string())
        );
        // Round 3: own write durable, no further change: stop silently
        // (no second "done", no "don't need update").
        assert_eq!(
            observer_round(true, RET_FAILED, "a,a2", "a2,b,c,d"),
            RoundAction::Converged
        );
    }

    /// First-round log parity: Unchanged keeps Go's "don't need update"
    /// branch, SkipResumed/UnknownType/BadMap map to their Go exits.
    #[test]
    fn converge_round_keeps_go_first_pass_branches() {
        assert_eq!(
            observer_round(false, RET_FAILED, "a,a2", "a2,b,c"),
            RoundAction::DontNeedUpdate
        );
        assert_eq!(
            observer_round(false, RET_RESUMED, "a,a2", "a,b,c"),
            RoundAction::Silent
        );
        assert_eq!(
            observer_round(false, "other", "a,a2", "a,b,c"),
            RoundAction::Silent
        );
        assert_eq!(
            observer_round(false, RET_FAILED, "solo", "a,b"),
            RoundAction::BadMap(vec!["solo".to_string()])
        );
    }
}
