//! Read-only gate for the backup listener (`state::Mode::Backup`).
//!
//! The backup listener (Go `BackupServer`, mode "backup") serves reads
//! only: every command not in [`ALLOWED`] replies [`ERROR`] -- Redis
//! replica semantics. Go had no such gate (its backup server happily
//! executed writes against the backup store); this is the Rust-side
//! defense so the replication target can never be corrupted by a
//! misrouted client. Pure table + predicate, like `tx/keyspec.rs`.

/// Redis-standard replica error (verbatim, including the trailing dot).
pub const ERROR: &str = "READONLY You can't write against a read only replica.";

/// Read-only commands the backup listener may run: protocol/meta commands,
/// pure reads, and the MULTI controls (writes are still rejected at
/// dispatch, including EXEC replays). Everything else mutates the store
/// (or the raft/cluster admin plane) and is denied.
pub const ALLOWED: &[&str] = &[
    // Protocol / meta.
    "ping",
    "quit",
    "echo",
    "select",
    "command",
    "info",
    "dbsize",
    "config",
    "asking",
    // String reads.
    "get",
    "mget",
    "strlen",
    "getrange",
    "getbit",
    "bitcount",
    "bitpos",
    // Key reads.
    "exists",
    "type",
    "ttl",
    "pttl",
    "scan",
    "keys",
    "randomkey",
    // Hash reads.
    "hget",
    "hmget",
    "hlen",
    "hexists",
    "hstrlen",
    "hgetall",
    "hkeys",
    "hvals",
    "hrandfield",
    "hscan",
    // Set reads.
    "smembers",
    "sismember",
    "smismember",
    "scard",
    "srandmember",
    "sscan",
    "sdiff",
    "sinter",
    "sunion",
    "sintercard",
    // List reads.
    "lindex",
    "llen",
    "lrange",
    "lpos",
    // ZSet reads.
    "zcard",
    "zscore",
    "zmscore",
    "zcount",
    "zrank",
    "zrevrank",
    "zrandmember",
    "zrange",
    "zrevrange",
    "zrangebyscore",
    "zrevrangebyscore",
    "zrangebylex",
    "zrevrangebylex",
    "zlexcount",
    "zscan",
    // Stream reads (`xread` without GROUP is a pure scan; the consumer
    // group verbs `xreadgroup`/`xack`/... mutate the PEL and stay out).
    // XIDLE stays out entirely: its `XIDLE <stream> <secs>` form takes
    // the latch and persists stream meta + TTL entries (`lite::append`),
    // and the gate is name-based, so the bare query form is denied too.
    "xlen",
    "xrange",
    "xrevrange",
    "xinfo",
    "xpending",
    "xread",
    // Vector-set reads.
    "vcard",
    "vdim",
    "vgetattr",
    "vsim",
    // Transaction controls (queue-time gate still rejects queued writes).
    "multi",
    "exec",
    "discard",
    "watch",
    "unwatch",
];

/// Whether `cmd` (lowercase) may run on the backup listener.
pub fn allowed(cmd: &str) -> bool {
    ALLOWED.contains(&cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry invariant: every allowlisted name is a real command, so
    /// the gate never accidentally "allows" something the dispatcher
    /// would have rejected as unknown (and, conversely, the backup error
    /// text is only ever sent for real commands).
    #[test]
    fn allowed_table_resolves_through_the_registry() {
        for name in ALLOWED {
            assert!(
                crate::command::lookup(name).is_some(),
                "ALLOWED lists '{name}' but lookup() does not know it"
            );
        }
    }

    /// Representative denials: every write/mutating family, the admin
    /// plane, and the sneaky read-lookalikes (SPOP/LPOP pop, GETDEL and
    /// SETRANGE rewrite, XADD/XREADGROUP touch the stream/PEL; XIDLE's
    /// `<secs>` form persists stream meta + TTL entries).
    #[test]
    fn mutating_commands_are_denied() {
        for cmd in [
            "set",
            "setnx",
            "getdel",
            "setrange",
            "incr",
            "del",
            "expire",
            "rename",
            "restore",
            "flushdb",
            "bitop",
            "spop",
            "smove",
            "lpop",
            "lmove",
            "blmove",
            "rpoplpush",
            "brpoplpush",
            "xadd",
            "xidle",
            "xreadgroup",
            "xack",
            "xpick",
            "cluster",
            "raft",
            "migrate",
        ] {
            assert!(
                !allowed(cmd),
                "'{cmd}' must be denied on the backup listener"
            );
        }
    }

    #[test]
    fn empty_and_unknown_names_are_denied() {
        assert!(!allowed(""));
        assert!(!allowed("setex"));
        assert!(!allowed("readonly"));
        assert!(!allowed("SET"));
    }
}
