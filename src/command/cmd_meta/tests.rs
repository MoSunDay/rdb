use super::*;

#[cfg(test)]
mod registry_sync_tests {
    use super::COMMANDS;

    /// Every metadata row must resolve to a registered handler: the table
    /// is the single source for COMMAND replies, so a stale row would lie
    /// to clients about a command this server cannot run.
    #[test]
    fn every_meta_row_is_registered() {
        for m in COMMANDS {
            assert!(
                crate::command::lookup(m.name).is_some(),
                "cmd_meta lists '{}' but lookup() has no handler",
                m.name
            );
        }
    }

    /// The registration count must equal the table count: a handler added
    /// without metadata would be invisible to COMMAND/COMMAND INFO.
    /// (Verified by count canary; update when adding commands.)
    #[test]
    fn table_covers_registry_count() {
        let mut n = 0usize;
        for name in ALL_REGISTERED {
            if crate::command::lookup(name).is_some() {
                n += 1;
            } else {
                panic!("lookup() lost '{name}'");
            }
        }
        assert_eq!(n, COMMANDS.len(), "registry and cmd_meta sizes diverge");
    }

    /// Names the registry is known to carry; grows with new commands.
    const ALL_REGISTERED: &[&str] = &[
        "append",
        "asking",
        "bitcount",
        "bitop",
        "bitpos",
        "blmove",
        "blpop",
        "brpop",
        "brpoplpush",
        "bzpopmax",
        "bzpopmin",
        "cluster",
        "command",
        "config",
        "dbsize",
        "decr",
        "decrby",
        "del",
        "discard",
        "echo",
        "exec",
        "exists",
        "expire",
        "expireat",
        "flushdb",
        "ft.add",
        "ft.build",
        "ft.create",
        "ft.del",
        "ft.drop",
        "ft.dropindex",
        "ft.info",
        "ft.search",
        "get",
        "getbit",
        "getdel",
        "getrange",
        "getset",
        "hdel",
        "hexists",
        "hget",
        "hgetall",
        "hincrby",
        "hincrbyfloat",
        "hkeys",
        "hlen",
        "hmget",
        "hmset",
        "hrandfield",
        "hscan",
        "hset",
        "hsetnx",
        "hstrlen",
        "hvals",
        "incr",
        "incrby",
        "incrbyfloat",
        "info",
        "json.arrappend",
        "json.arrindex",
        "json.arrinsert",
        "json.arrlen",
        "json.arrpop",
        "json.arrtrim",
        "json.del",
        "json.forget",
        "json.get",
        "json.mget",
        "json.numincrby",
        "json.objkeys",
        "json.objlen",
        "json.set",
        "json.strappend",
        "json.strlen",
        "json.type",
        "keys",
        "lindex",
        "lmpop",
        "linsert",
        "llen",
        "lmove",
        "lpop",
        "lpos",
        "lpush",
        "lpushx",
        "lrange",
        "lrem",
        "lset",
        "ltrim",
        "mget",
        "migrate",
        "mset",
        "multi",
        "persist",
        "pexpire",
        "pexpireat",
        "ping",
        "psetex",
        "pttl",
        "quit",
        "raft",
        "randomkey",
        "rename",
        "renamenx",
        "restore",
        "rpop",
        "rpoplpush",
        "rpush",
        "rpushx",
        "sadd",
        "scan",
        "scard",
        "sdiff",
        "sdiffstore",
        "select",
        "set",
        "setbit",
        "setex",
        "setnx",
        "setrange",
        "sinter",
        "sintercard",
        "sinterstore",
        "sismember",
        "smembers",
        "smismember",
        "smove",
        "spop",
        "srandmember",
        "srem",
        "sscan",
        "strlen",
        "sunion",
        "sunionstore",
        "ttl",
        "type",
        "unlink",
        "unwatch",
        "vadd",
        "vcard",
        "vdim",
        "vgetattr",
        "vrem",
        "vsetattr",
        "vsim",
        "watch",
        "xack",
        "xadd",
        "xautoclaim",
        "xclaim",
        "xdel",
        "xgroup",
        "xidle",
        "xinfo",
        "xlen",
        "xpending",
        "xpick",
        "xrange",
        "xread",
        "xreadgroup",
        "xrevrange",
        "xtrim",
        "zadd",
        "zcard",
        "zcount",
        "zdiffstore",
        "zincrby",
        "zinterstore",
        "zlexcount",
        "zmscore",
        "zpopmax",
        "zpopmin",
        "zrandmember",
        "zrange",
        "zrangebylex",
        "zrangebyscore",
        "zrank",
        "zrem",
        "zremrangebylex",
        "zremrangebyrank",
        "zremrangebyscore",
        "zrevrange",
        "zrevrangebylex",
        "zrevrangebyscore",
        "zrevrank",
        "zscan",
        "zscore",
        "zunionstore",
    ];
}

#[cfg(test)]
mod lookup_tests {
    use super::*;

    #[test]
    fn no_duplicate_names_and_nonzero_arity() {
        let mut names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate command names in COMMANDS");
        for c in COMMANDS {
            assert_ne!(c.arity, 0, "{} has arity 0", c.name);
        }
    }

    #[test]
    fn lookup_meta_matches_case_insensitively() {
        assert_eq!(lookup_meta(b"get").unwrap().name, "get");
        assert_eq!(lookup_meta(b"GET").unwrap().name, "get");
        assert_eq!(lookup_meta(b"Json.Get").unwrap().name, "json.get");
        assert!(lookup_meta(b"no-such-command").is_none());
    }
}

/// Cross-table drift seal (routing vs queue key extraction).
/// `router::routing_key_index` picks the argv position both dispatch and
/// the MULTI queue resolve the slot prefix from; `tx::keyspec::keys_of`
/// independently extracts the key set for queue-time single-slot checks.
/// The two tables evolve separately (plus `is_whitelisted`, the third
/// input), so this hermetic test pins their agreement for EVERY
/// registered command: the routing position must be one of the keys
/// `keys_of` extracts, and a command with no keys at all must be
/// route-exempt. New commands are covered the day they gain a metadata
/// row (registry_sync_tests keeps rows == registry in lockstep).
mod routing_consistency_tests {
    use crate::command::cmd_meta::COMMANDS;
    use crate::router;
    use crate::tx::keyspec::{self, Shape};

    /// Shape-faithful sample argv[1..] per family: positions that are
    /// keys carry distinct `kN` tokens mirroring each family's real
    /// grammar, so the membership check below is meaningful.
    fn sample_args(shape: Shape) -> Vec<Vec<u8>> {
        fn key(n: u8) -> Vec<u8> {
            format!("k{n}").into_bytes()
        }
        match shape {
            Shape::None => vec![],
            Shape::First => vec![key(1)],
            Shape::All => vec![key(1), key(2)],
            Shape::Even => vec![key(1), b"v1".to_vec(), key(2), b"v2".to_vec()],
            Shape::FirstTwo => vec![key(1), key(2)],
            Shape::Second => vec![b"sub".to_vec(), key(1)],
            Shape::Skip1 => vec![b"AND".to_vec(), key(1), key(2)],
            Shape::NumKeys => vec![b"2".to_vec(), key(1), key(2), b"LEFT".to_vec()],
            Shape::ZStore => vec![
                key(1),
                b"2".to_vec(),
                key(2),
                key(3),
                b"AGGREGATE".to_vec(),
                b"sum".to_vec(),
            ],
        }
    }

    #[test]
    fn routing_key_position_is_also_a_queue_key() {
        for meta in COMMANDS {
            let cmd = meta.name;
            let shape = keyspec::shape_of(cmd);
            if shape == Shape::None {
                // No keys anywhere: slot routing must be bypassed, else
                // dispatch would prefix-hash a flag/cursor/subcommand
                // token as if it were a key.
                assert!(
                    router::is_whitelisted(cmd),
                    "'{cmd}' extracts no queue keys but is not route-exempt"
                );
                continue;
            }
            if router::is_whitelisted(cmd) {
                continue; // streams / admin: no routing key is consulted
            }
            let mut argv: Vec<Vec<u8>> = vec![cmd.as_bytes().to_vec()];
            argv.extend(sample_args(shape));
            let idx = router::routing_key_index(cmd);
            assert!(
                idx < argv.len(),
                "'{cmd}': routing index {idx} outside sampled argv"
            );
            let routing_key = &argv[idx];
            let keys = keyspec::keys_of(cmd, &argv[1..]);
            assert!(
                !keys.is_empty(),
                "'{cmd}' routes by key but keys_of extracted none"
            );
            let show = |k: &[u8]| String::from_utf8_lossy(k).into_owned();
            assert!(
                keys.iter().any(|k| k == routing_key),
                "'{cmd}': routing key argv[{idx}]='{}' is not among keys_of keys [{}]",
                show(routing_key),
                keys.iter().map(|k| show(k)).collect::<Vec<_>>().join(", ")
            );
        }
    }
}
