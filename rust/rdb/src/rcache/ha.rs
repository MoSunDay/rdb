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
//! - Go's leader-notify goroutine only toggles the unread HTTP
//!   `ENABLE_WRITE` flag: intentionally skipped.
//! - Go's `strings.Replace`/`strings.Contains` on the instances list are
//!   SUBSTRING operations and corrupt overlapping addresses ("10.0.0.1"
//!   also matches inside "10.0.0.11"); the port replaces/matches exact
//!   comma-separated elements and rejects empty src/target map halves as
//!   BadMap.

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

/// Go `updateClusterStableSlots`: log+stop when equal; else apply and
/// ALWAYS log "done" afterwards, even on apply failure (Go ordering).
async fn update_cluster_stable_slots(
    raft: &RdbRaft,
    new_instances: &str,
    current_instances: &str,
    ret_type: &str,
    peer_addr: &str,
) {
    if new_instances == current_instances {
        eprintln!("{ret_type} {peer_addr} don't need update");
        return;
    }
    if let Err(e) = raft_apply(raft, INSTANCES_KEY, new_instances).await {
        eprintln!("raft.Apply failed:{e}");
    }
    eprintln!("{ret_type} {peer_addr} done");
}

/// Go `handlerObserver`: read backup map + instances from the live FSM,
/// then fail over (or back) via raft.
pub async fn handler_observer(raft: Arc<RdbRaft>, kv: KvMap, ret_type: &str, peer_addr: &str) {
    let backup_val = kv_get(&kv, &format!("backup_target_map_{peer_addr}"));
    let instances = kv_get(&kv, INSTANCES_KEY);
    match decide(ret_type, &backup_val, &instances) {
        FailoverDecision::Replace(new) => {
            update_cluster_stable_slots(&raft, &new, &instances, ret_type, peer_addr).await;
        }
        FailoverDecision::Unchanged => {
            eprintln!("{ret_type} {peer_addr} don't need update");
        }
        FailoverDecision::BadMap(parts) => {
            eprintln!("failedNodeBackupMap error: {parts:?}");
        }
        FailoverDecision::SkipResumed | FailoverDecision::UnknownType => {}
    }
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
async fn self_recover(raft: &RdbRaft, kv: &KvMap, self_addr: &str) {
    let backup_val = kv_get(kv, &format!("backup_target_map_{self_addr}"));
    let instances = kv_get(kv, INSTANCES_KEY);
    let parts: Vec<&str> = backup_val.split(',').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty()) {
        if !backup_val.is_empty() {
            eprintln!("failedNodeBackupMap error: {parts:?}");
        }
        return;
    }
    if contains_element(&instances, parts[1]) {
        let new = replace_element(&instances, parts[1], parts[0]);
        update_cluster_stable_slots(raft, &new, &instances, RET_RESUMED, parts[0]).await;
    }
}

/// Go leader probe: every 5s, while ready and leading, verify leadership,
/// heal the own failover entry, probe voters (success->Resumed, fail->Failed).
pub fn spawn_leader_probe(
    raft: Arc<RdbRaft>,
    kv: KvMap,
    topo: Arc<RwLock<topology::Topology>>,
    self_addr: String,
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
            self_recover(&raft, &kv, &self_addr).await;

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
                        handler_observer(raft.clone(), kv.clone(), RET_RESUMED, &peer).await;
                    }
                    Err(e) => {
                        eprintln!("rcache heartbeat failed err:{e}");
                        handler_observer(raft.clone(), kv.clone(), RET_FAILED, &peer).await;
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
/// leader seeds `backup_target_map_<k>` + sentinel (apply error: exit(1)).
pub fn spawn_backup_map_init(raft: Arc<RdbRaft>, kv: KvMap, conf: &conf::Config) {
    // Nothing to seed: Go would loop forever without applying anything.
    if conf.backup_target_map.is_empty() {
        return;
    }
    let entries = backup_map_entries(&conf.backup_target_map);
    let self_addr = conf.raft_tcp_address.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INIT_INTERVAL);
        loop {
            ticker.tick().await; // Go order: sleep 1s, read sentinel up front,
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
                    eprintln!("raft.Apply backup_target_map failed:{e}");
                    std::process::exit(1);
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
        let got = failover_action(
            RET_FAILED,
            "10.0.0.1,10.0.0.2",
            "10.0.0.1,10.0.0.11",
        );
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
}
