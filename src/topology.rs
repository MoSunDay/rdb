//! Cluster topology state: routing tables refreshed from the raft key
//! `cluster_slots_stable_instances` (Go syncs it every 3s in
//! `internal/server/server.go`).
//!
//! Slot ownership can be overridden per slot by the raft key
//! `slot_owner_map` (JSON `{"<slot>":"<resp addr>"}`): after a migration
//! the new owner's addr is authoritative for that slot and the equal-split
//! band only applies to unlisted slots (`router::route_with_owners`).
//!
//! AGREED BUG FIX for the rewrite: `cluster_ready` simply means "the
//! instances list is non-empty". The Go second clause
//! `(len(addrs)%2 != 0 && addrs[0] != "")` is dead because the first
//! operand `instances != ""` already dominates the `||`; the fix just makes
//! the intent explicit.

use std::collections::HashMap;

/// Total number of hash slots in a Redis cluster.
pub const SLOT_NUMBER: usize = 16384;

/// Routing state derived from the stable instance list.
#[derive(Clone, Debug, PartialEq)]
pub struct Topology {
    /// True once a non-empty instance list has been observed.
    pub cluster_ready: bool,
    /// Stable instance addresses in slot-assignment order.
    pub stable_addrs: Vec<String>,
    /// Slots per node: `SLOT_NUMBER / len(stable_addrs)` (integer division).
    pub per_node_slots: usize,
    /// Post-migration ownership overrides: slot -> owning RESP addr.
    /// Authoritative over the equal-split band (unlisted slots only).
    pub owner_map: HashMap<u16, String>,
}

/// A topology with no cluster state (not ready, no addrs, zero slots/node).
pub fn empty() -> Topology {
    Topology {
        cluster_ready: false,
        stable_addrs: Vec::new(),
        per_node_slots: 0,
        owner_map: HashMap::new(),
    }
}

/// Rebuild routing state from the raft instance list.
///
/// Mirrors the Go refresh: `instances == ""` clears readiness; otherwise the
/// comma-separated list becomes `stable_addrs` and each node is assigned
/// `SLOT_NUMBER / len` slots (integer division, matching Go).
pub fn refresh(instances: &str) -> Topology {
    if instances.is_empty() {
        return empty();
    }
    let addrs: Vec<String> = instances.split(',').map(|a| a.to_string()).collect();
    let per = SLOT_NUMBER / addrs.len();
    Topology {
        cluster_ready: true,
        stable_addrs: addrs,
        per_node_slots: per,
        owner_map: HashMap::new(),
    }
}

/// Serialize the per-slot ownership overrides for the `slot_owner_map`
/// raft value (JSON `{"<slot>":"<addr>",...}`, sorted by slot for
/// deterministic diffs).
pub fn owner_map_json(map: &HashMap<u16, String>) -> String {
    let mut entries: Vec<(&u16, &String)> = map.iter().collect();
    entries.sort_by_key(|(slot, _)| **slot);
    let body: Vec<String> = entries
        .iter()
        .map(|(slot, addr)| format!("\"{slot}\":\"{addr}\""))
        .collect();
    format!("{{{}}}", body.join(","))
}

/// Raft key holding the per-slot ownership overrides (JSON map).
pub const OWNER_MAP_KEY: &str = "slot_owner_map";

/// Parse the `slot_owner_map` raft value (JSON `{"<slot>":"<addr>"}`,
/// slots as decimal strings). Malformed values yield an empty map: routing
/// then falls back to the equal-split bands, which is the pre-migration
/// state (safe -- the same key names the raft value writer uses).
pub fn parse_owner_map(json: &str) -> HashMap<u16, String> {
    let Ok(map) = serde_json::from_str::<HashMap<String, String>>(json) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (slot, addr) in map {
        if let Ok(s) = slot.parse::<u16>() {
            if s < SLOT_NUMBER as u16 && !addr.is_empty() {
                out.insert(s, addr);
            }
        }
    }
    out
}

/// [`refresh`] plus the ownership overrides from `slot_owner_map`.
pub fn refresh_with_owners(instances: &str, owner_map_json: &str) -> Topology {
    let mut t = refresh(instances);
    t.owner_map = parse_owner_map(owner_map_json);
    t
}

/// Per-node displayed slot ranges used by `cluster nodes` / `cluster slots`.
///
/// Follows Go `getNodeSlots()`: non-last nodes get explicit `start-end`
/// bands of `SLOT_NUMBER / len` slots; the last node is rendered as
/// `start..SLOT_NUMBER-1`, i.e. it always ends at 16383 and (approved fix)
/// starts at the running `start`. The Go version rendered the last node as
/// `end+1..16383`, which for a single node (end == 0) produced `"1-16383"`
/// and silently omitted slot 0; that off-by-one is fixed here so a
/// single-node cluster reports the full `0-16383` range.
pub fn parse_node_slots(addrs: &[String]) -> HashMap<String, String> {
    let mut slots = HashMap::new();
    if addrs.is_empty() {
        return slots; // Go would divide by zero; guard to an empty map.
    }
    let per = SLOT_NUMBER / addrs.len();
    let mut start = 0usize;
    for (index, addr) in addrs.iter().enumerate() {
        if index == addrs.len() - 1 {
            // Last node: runs through slot 16383; `start` is where the
            // previous band ended plus one (0 for a single node).
            slots.insert(addr.clone(), format!("{}-{}", start, SLOT_NUMBER - 1));
        } else {
            let end = per * (index + 1);
            slots.insert(addr.clone(), format!("{}-{}", start, end));
            start = end + 1;
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_topology_not_ready() {
        let t = empty();
        assert!(!t.cluster_ready);
        assert!(t.stable_addrs.is_empty());
        assert_eq!(t.per_node_slots, 0);
    }

    #[test]
    fn refresh_empty_string_not_ready() {
        let t = refresh("");
        assert_eq!(t, empty());
    }

    #[test]
    fn refresh_three_nodes() {
        let t = refresh("a,b,c");
        assert!(t.cluster_ready);
        assert_eq!(t.stable_addrs, strings(&["a", "b", "c"]));
        assert_eq!(t.per_node_slots, 5461); // 16384/3 integer division
    }

    #[test]
    fn refresh_single_node() {
        let t = refresh("only");
        assert!(t.cluster_ready);
        assert_eq!(t.stable_addrs, strings(&["only"]));
        assert_eq!(t.per_node_slots, 16384);
    }

    #[test]
    fn refresh_two_nodes() {
        let t = refresh("a,b");
        assert!(t.cluster_ready);
        assert_eq!(t.per_node_slots, 8192);
    }

    #[test]
    fn node_slots_three_nodes_exact_ranges() {
        let m = parse_node_slots(&strings(&["a", "b", "c"]));
        assert_eq!(m.get("a").unwrap(), "0-5461");
        assert_eq!(m.get("b").unwrap(), "5462-10922");
        assert_eq!(m.get("c").unwrap(), "10923-16383");
    }

    #[test]
    fn node_slots_single_node_full_range() {
        // Approved fix: a single node reports the full 0-16383 range (the
        // Go version omitted slot 0 with "1-16383").
        let m = parse_node_slots(&strings(&["solo"]));
        assert_eq!(m.get("solo").unwrap(), "0-16383");
    }

    #[test]
    fn node_slots_two_nodes() {
        let m = parse_node_slots(&strings(&["a", "b"]));
        assert_eq!(m.get("a").unwrap(), "0-8192");
        assert_eq!(m.get("b").unwrap(), "8193-16383");
    }

    #[test]
    fn node_slots_empty_is_empty_map() {
        let m = parse_node_slots(&[]);
        assert!(m.is_empty());
    }

    #[test]
    fn node_slots_five_nodes_last_node_reaches_16383() {
        // per = 16384/5 = 3276. Non-last nodes get start-end bands; the last
        // node is rendered from the running end so it always ends at 16383.
        let m = parse_node_slots(&strings(&["a", "b", "c", "d", "e"]));
        assert_eq!(m.get("a").unwrap(), "0-3276");
        assert_eq!(m.get("b").unwrap(), "3277-6552");
        assert_eq!(m.get("c").unwrap(), "6553-9828");
        assert_eq!(m.get("d").unwrap(), "9829-13104");
        assert_eq!(m.get("e").unwrap(), "13105-16383");
    }
    #[test]
    fn parse_owner_map_roundtrip_and_validation() {
        let m = parse_owner_map(r#"{"100":"b:1","99999":"x:1","":"y:1","12":"c:1"}"#);
        assert_eq!(m.len(), 2, "out-of-range and empty addr entries dropped");
        assert_eq!(m.get(&100), Some(&"b:1".to_string()));
        assert_eq!(m.get(&12), Some(&"c:1".to_string()));
        assert!(parse_owner_map("not json").is_empty());
        assert!(parse_owner_map("").is_empty());
    }

    #[test]
    fn refresh_with_owners_carries_overrides() {
        let t = refresh_with_owners("a,b,c", r#"{"5":"b:1"}"#);
        assert_eq!(t.stable_addrs, strings(&["a", "b", "c"]));
        assert_eq!(t.owner_map.get(&5), Some(&"b:1".to_string()));
        assert!(!t.owner_map.contains_key(&6));
    }
}
