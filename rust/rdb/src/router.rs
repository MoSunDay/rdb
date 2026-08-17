//! MOVED routing: pure functions replicating the Go server's slot dispatch
//! (`internal/server/server.go`) for non-whitelisted commands.
//!
//! The Go loop picks the first node whose inclusive upper bound covers the
//! slot: `slot <= (index+1)*perNodeslots`. If that node is `host` the
//! command is served locally; otherwise the client is redirected with
//! `MOVED <slot> <addr>`. If no node covers the slot (empty addrs, or a
//! remainder slot beyond `len*per`) the Go code falls through and serves
//! locally - that quirk is preserved here.

/// Outcome of routing one slot against the stable topology.
#[derive(Clone, Debug, PartialEq)]
pub enum RouteDecision {
    /// Serve on this instance (owner is `host`, or no node matched).
    Local,
    /// Redirect the client to `addr`, which owns `slot`.
    Moved { slot: u16, addr: String },
}

/// Route `slot` across `addrs`, each owning `per_node_slots` slots.
///
/// Mirrors the Go loop exactly: the first index satisfying the inclusive
/// `slot <= (index+1)*per_node_slots` boundary wins. `host` == owner means
/// local. Falling off the end (no match) returns `Local`, matching Go.
pub fn route(slot: u16, addrs: &[String], per_node_slots: usize, host: &str) -> RouteDecision {
    let slot = slot as usize;
    for (index, addr) in addrs.iter().enumerate() {
        if slot <= (index + 1) * per_node_slots {
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
    // No node matched (empty addrs or remainder beyond len*per): Go serves
    // locally by falling through the loop.
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
    fn five_nodes_per_3276_remainder_quirk() {
        // 5 * 3276 == 16380, so slots 16381..=16383 match no node and the
        // Go loop falls through to local service (documented quirk).
        let a = addrs(5);
        expect_node(16380, &a, 3276, 4);
        expect_local(16381, &a, 3276);
        expect_local(16382, &a, 3276);
        expect_local(16383, &a, 3276);
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
