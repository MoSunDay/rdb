//! MOVED routing: pure functions replicating the Go server's slot dispatch
//! (`internal/server/server.go`) for non-whitelisted commands.
//!
//! The Go loop picks the first node whose inclusive upper bound covers the
//! slot: `slot <= (index+1)*perNodeslots`. If that node is `host` the
//! command is served locally; otherwise the client is redirected with
//! `MOVED <slot> <addr>`. APPROVED BREAKING FIX: when 16384 % N != 0 the
//! leftover slots matched no node in Go (and were served locally on
//! whichever node received them); here the LAST node owns everything
//! through slot 16383, so every slot has exactly one owner.

/// Outcome of routing one slot against the stable topology.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteDecision {
    /// Serve on this instance (owner is `host`, or `addrs` is empty).
    Local,
    /// Redirect the client to `addr`, which owns `slot`.
    Moved { slot: u16, addr: String },
}

/// Route `slot` across `addrs`, each owning `per_node_slots` slots.
///
/// The first index satisfying the inclusive `slot <= (index+1)*per_node_slots`
/// boundary wins; `host` == owner means local. APPROVED BREAKING FIX: the
/// LAST node additionally owns every slot beyond `len*per_node_slots`, so
/// coverage is total (through 16383) and ranges stay disjoint. Only empty
/// `addrs` falls through to `Local`, matching Go's empty-list behavior.
pub fn route(slot: u16, addrs: &[String], per_node_slots: usize, host: &str) -> RouteDecision {
    let slot = slot as usize;
    let last = addrs.len().saturating_sub(1);
    for (index, addr) in addrs.iter().enumerate() {
        // The last node absorbs the remainder when 16384 % N != 0.
        if index == last || slot <= (index + 1) * per_node_slots {
            return if addr == host {
                RouteDecision::Local
            } else {
                RouteDecision::Moved {
                    slot: slot as u16,
                    addr: addr.clone(),
                }
            };
        }
    }
    // No nodes at all: Go serves locally by falling through the loop.
    RouteDecision::Local
}

/// Format the MOVED redirect line: `MOVED <slot> <addr>`.
pub fn moved_error_line(slot: u16, addr: &str) -> String {
    format!("MOVED {} {}", slot, addr)
}

/// Commands that bypass slot routing entirely (Go `command.Whitelist`,
/// extended for the keyless key-space commands: SCAN's first arg is a
/// cursor and KEYS' is a pattern, neither is a routing key; RANDOMKEY has
/// no args. With no routing key these scan the whole local keyspace).
pub fn is_whitelisted(cmd_lowercase: &str) -> bool {
    matches!(
        cmd_lowercase,
        "ping"
            | "quit"
            | "config"
            | "cluster"
            | "raft"
            | "migrate"
            | "scan"
            | "keys"
            | "randomkey"
            | "xadd"
            | "xlen"
            | "xrange"
            | "xtrim"
            | "xdel"
            | "xidle"
            | "xread"
            | "xreadgroup"
            | "xack"
            | "xgroup"
            | "xinfo"
            | "xpick"
            | "xpending"
            | "xclaim"
            | "xautoclaim"
            // transaction controls: no keys to route (WATCH is NOT here --
            // it routes like any keyed command so it can record the key's
            // slot prefix and honor MOVED)
            | "multi"
            | "exec"
            | "discard"
            | "unwatch"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("n{}", i)).collect()
    }

    /// Expect the slot to be owned by node `want` (and thus MOVED there,
    /// since `host` never equals any generated addr).
    fn expect_node(slot: u16, list: &[String], per: usize, want: usize) {
        let got = route(slot, list, per, "self");
        let want_addr = format!("n{}", want);
        assert_eq!(
            got,
            RouteDecision::Moved {
                slot,
                addr: want_addr
            },
            "slot {} per {} expected node {}",
            slot,
            per,
            want
        );
    }

    fn expect_local(slot: u16, list: &[String], per: usize) {
        assert_eq!(
            route(slot, list, per, "self"),
            RouteDecision::Local,
            "slot {} per {} expected Local",
            slot,
            per
        );
    }

    #[test]
    fn three_nodes_per_5461() {
        let a = addrs(3);
        // node0 owns [0, 5461]
        expect_node(0, &a, 5461, 0);
        expect_node(5461, &a, 5461, 0);
        // node1 owns [5462, 10922]
        expect_node(5462, &a, 5461, 1);
        expect_node(10922, &a, 5461, 1);
        // node2 owns [10923, 16383]
        expect_node(10923, &a, 5461, 2);
        expect_node(16383, &a, 5461, 2);
    }

    #[test]
    fn two_nodes_per_8192() {
        let a = addrs(2);
        expect_node(0, &a, 8192, 0);
        expect_node(8192, &a, 8192, 0);
        expect_node(8193, &a, 8192, 1);
        expect_node(16383, &a, 8192, 1);
    }

    #[test]
    fn one_node_per_16384() {
        let a = addrs(1);
        expect_node(0, &a, 16384, 0);
        expect_node(16383, &a, 16384, 0);
    }

    #[test]
    fn five_nodes_last_node_absorbs_remainder() {
        // 5 * 3276 == 16380, so slots 16380..=16383 are remainder slots:
        // the LAST node owns them (approved fix; Go served them locally on
        // whichever node received the request).
        let a = addrs(5);
        // 4*3276 == 13104 is node 3's inclusive upper bound.
        expect_node(13104, &a, 3276, 3);
        expect_node(13105, &a, 3276, 4);
        expect_node(16380, &a, 3276, 4);
        expect_node(16381, &a, 3276, 4);
        expect_node(16382, &a, 3276, 4);
        expect_node(16383, &a, 3276, 4);
    }

    /// Owner index of `slot`; `host` is "self", which never matches any
    /// generated addr, so the decision is always a MOVED to the owner.
    fn owner_of(slot: u16, list: &[String], per: usize) -> usize {
        match route(slot, list, per, "self") {
            RouteDecision::Moved { addr, .. } => addr[1..].parse::<usize>().unwrap(),
            RouteDecision::Local => panic!("slot {} has no owner", slot),
        }
    }

    #[test]
    fn every_slot_owned_for_one_to_seventeen_nodes() {
        // Total coverage with disjoint, ordered bands for any node count,
        // including every N in 1..=17 where 16384 % N != 0.
        for n in 1..=17usize {
            let a = addrs(n);
            let per = 16384 / n;
            let mut prev = 0usize;
            for slot in 0..=16383u16 {
                let owner = owner_of(slot, &a, per);
                assert!(
                    owner < n,
                    "n={} slot {} owner {} out of range",
                    n,
                    slot,
                    owner
                );
                assert!(
                    owner >= prev,
                    "n={} slot {} owner {} regressed below {}",
                    n,
                    slot,
                    owner,
                    prev
                );
                prev = owner;
            }
            assert_eq!(owner_of(0, &a, per), 0);
            assert_eq!(owner_of(16383, &a, per), n - 1);
        }
    }

    #[test]
    fn exactly_one_local_owner_for_representative_node_counts() {
        // For every slot exactly one node serves locally and every other
        // node redirects to that same owner (no overlap, no gaps).
        for n in [2usize, 3, 5, 7, 17] {
            let a = addrs(n);
            let per = 16384 / n;
            for slot in 0..=16383u16 {
                let mut locals = 0usize;
                let mut owner: Option<usize> = None;
                for addr in a.iter() {
                    match route(slot, &a, per, addr) {
                        RouteDecision::Local => locals += 1,
                        RouteDecision::Moved { addr: to, .. } => {
                            let idx = to[1..].parse::<usize>().unwrap();
                            assert!(idx < n, "n={} slot {} redirects to bogus {}", n, slot, idx);
                            if let Some(seen) = owner {
                                assert_eq!(seen, idx, "n={} slot {} redirects disagree", n, slot);
                            } else {
                                owner = Some(idx);
                            }
                        }
                    }
                }
                assert_eq!(
                    locals, 1,
                    "n={} slot {} must have exactly one local owner",
                    n, slot
                );
                let owner = owner.unwrap_or_else(|| panic!("n={} slot {} unowned", n, slot));
                assert_eq!(a[owner], format!("n{}", owner));
            }
        }
    }

    #[test]
    fn five_nodes_slot_16383_maps_to_last_node() {
        let a = addrs(5);
        assert_eq!(owner_of(16383, &a, 3276), 4);
    }

    #[test]
    fn owner_is_host_serves_locally() {
        let a = addrs(3);
        // slot 0 belongs to n0; pretending we are n0 serves it locally.
        assert_eq!(route(0, &a, 5461, "n0"), RouteDecision::Local);
        assert_eq!(route(5461, &a, 5461, "n0"), RouteDecision::Local);
        // middle node owns its band too.
        assert_eq!(route(10922, &a, 5461, "n1"), RouteDecision::Local);
        // but a slot owned by another node still redirects.
        assert_eq!(
            route(5462, &a, 5461, "n0"),
            RouteDecision::Moved {
                slot: 5462,
                addr: "n1".to_string()
            }
        );
    }

    #[test]
    fn empty_addrs_is_local() {
        let none: Vec<String> = Vec::new();
        expect_local(0, &none, 5461);
        expect_local(16383, &none, 5461);
    }

    #[test]
    fn moved_line_format() {
        assert_eq!(
            moved_error_line(5465, "127.0.0.1:32681"),
            "MOVED 5465 127.0.0.1:32681"
        );
        assert_eq!(moved_error_line(0, "a:1"), "MOVED 0 a:1");
        assert_eq!(moved_error_line(16383, "z:9"), "MOVED 16383 z:9");
    }

    #[test]
    fn whitelist_membership() {
        for c in [
            "ping",
            "quit",
            "config",
            "cluster",
            "raft",
            "migrate",
            "scan",
            "keys",
            "randomkey",
            "xadd",
            "xlen",
            "xrange",
            "xtrim",
            "xdel",
            "xidle",
            "xread",
            "xreadgroup",
            "xack",
            "xgroup",
            "xinfo",
            "xpick",
            "xpending",
            "xclaim",
            "xautoclaim",
        ] {
            assert!(is_whitelisted(c), "{} should be whitelisted", c);
        }
        for c in [
            "get", "set", "del", "ping ", "PING", "", "raftx", "expire", "ttl",
        ] {
            assert!(!is_whitelisted(c), "{} should NOT be whitelisted", c);
        }
    }
}
