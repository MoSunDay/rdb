//! Queue-time key extraction for MULTI transactions.
//!
//! To keep a queued transaction single-slot (cluster requirement) the
//! connection must know which argv words are KEYS when a command is
//! queued -- before any handler runs. This table covers every command
//! that deviates from the "first argument is the only key" default.
//!
//! Precision note: an over-broad extraction (a flag word counted as a
//! key) can only cause a spurious CROSSSLOT rejection at QUEUE time --
//! never a wrong execution. Handlers still validate their own arity and
//! layout at replay time, so a conservative table is safe.

/// How a command's argv maps to keys. `args` = argv[1..] (command name
/// excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// No keys at all (admin / full-table scans / blocking commands).
    None,
    /// Default: args[0] is the only key (also used for unlisted commands,
    /// matching how routing picks the slot from argv[1]).
    First,
    /// Every argument is a key.
    All,
    /// Key/value pairs: even-indexed arguments are keys (MSET k v k v).
    Even,
    /// Two keys: args[0] and args[1] (RENAME, MOVE-shaped commands).
    FirstTwo,
    /// Key at args[1], i.e. argv[2] (sub-command first: XGROUP/XINFO...).
    Second,
    /// DEST NUMKEYS KEY... -- Z{UNION,INTER,DIFF}STORE family.
    ZStore,
}

/// Static shape of a command; unlisted commands use [`Shape::First`].
pub fn shape_of(cmd: &str) -> Shape {
    match cmd {
        "ping" | "quit" | "config" | "cluster" | "raft" | "migrate" | "scan" | "keys"
        | "randomkey" | "xread" | "xreadgroup" => Shape::None,
        "xgroup" | "xinfo" | "xpick" | "xack" => Shape::Second, // xidle keys at argv[1]
        // Lite PEL verbs: key at argv[1] (args[0]).
        "xpending" | "xclaim" | "xautoclaim" => Shape::First,
        "rename" | "renamenx" | "smove" | "lmove" | "blmove" | "rpoplpush" | "brpoplpush" => {
            Shape::FirstTwo
        }
        "mset" => Shape::Even,
        "del" | "unlink" | "exists" | "mget" | "smismember" | "sdiff" | "sinter" | "sunion"
        | "sdiffstore" | "sinterstore" | "sunionstore" => Shape::All,
        "zunionstore" | "zinterstore" | "zdiffstore" => Shape::ZStore,
        _ => Shape::First,
    }
}

/// Extract the user keys of `cmd` (lowercase) from `args` (argv[1..]).
/// Empty/short argv yield an empty vector; the replay-time handler still
/// reports its own arity error inside the EXEC array.
pub fn keys_of(cmd: &str, args: &[Vec<u8>]) -> Vec<Vec<u8>> {
    match shape_of(cmd) {
        Shape::None => Vec::new(),
        Shape::First => args.first().map(|k| vec![k.clone()]).unwrap_or_default(),
        Shape::All => args.to_vec(),
        Shape::Even => args
            .chunks(2)
            .filter_map(|pair| pair.first().cloned())
            .collect(),
        Shape::FirstTwo => args.iter().take(2).cloned().collect(),
        Shape::Second => args.get(1).map(|k| vec![k.clone()]).unwrap_or_default(),
        Shape::ZStore => {
            // DEST NUMKEYS KEY [KEY ...] [WEIGHTS ...] [AGGREGATE ...]
            let Some(dest) = args.first() else {
                return Vec::new();
            };
            let mut keys = vec![dest.clone()];
            let Ok(numkeys) = std::str::from_utf8(args.get(1).map(|v| v.as_slice()).unwrap_or(b""))
                .map(|s| s.parse::<usize>())
            else {
                return keys;
            };
            let Ok(numkeys) = numkeys else {
                return keys;
            };
            for k in args.iter().skip(2).take(numkeys) {
                keys.push(k.clone());
            }
            keys
        }
    }
}

/// Commands that may park the connection; never queueable inside MULTI
/// (their reply depends on timing, breaking the queued-reply contract).
pub fn may_block(cmd: &str) -> bool {
    matches!(
        cmd,
        "blpop"
            | "brpop"
            | "blmove"
            | "brpoplpush"
            | "bzpopmin"
            | "bzpopmax"
            | "xread"
            | "xreadgroup"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn defaults_to_first() {
        assert_eq!(keys_of("get", &[b("k")]), vec![b("k")]);
        assert_eq!(keys_of("set", &[b("k"), b("v")]), vec![b("k")]);
        // unknown commands behave like First (routing-compatible)
        assert_eq!(keys_of("notacommand", &[b("k")]), vec![b("k")]);
    }

    #[test]
    fn none_shape() {
        assert!(keys_of("scan", &[b("0")]).is_empty());
        assert!(keys_of("keys", &[b("*")]).is_empty());
        assert!(keys_of("xread", &[b("STREAMS")]).is_empty());
    }

    #[test]
    fn all_and_even() {
        assert_eq!(
            keys_of("del", &[b("a"), b("b"), b("c")]),
            vec![b("a"), b("b"), b("c")]
        );
        // odd tail word counts as a key too: conservative, and a malformed
        // MSET is rejected by its own handler at replay time anyway
        assert_eq!(
            keys_of("mset", &[b("a"), b("1"), b("b"), b("2"), b("c")]),
            vec![b("a"), b("b"), b("c")]
        );
        assert_eq!(
            keys_of("sinterstore", &[b("dst"), b("s1"), b("s2")]),
            vec![b("dst"), b("s1"), b("s2")]
        );
    }

    #[test]
    fn two_key_and_second() {
        assert_eq!(keys_of("rename", &[b("a"), b("b")]), vec![b("a"), b("b")]);
        assert_eq!(
            keys_of("xgroup", &[b("CREATE"), b("s"), b("g")]),
            vec![b("s")]
        );
        assert_eq!(keys_of("xinfo", &[b("STREAM"), b("s")]), vec![b("s")]);
    }

    #[test]
    fn zstore_respects_numkeys() {
        let args = vec![b("dst"), b("2"), b("z1"), b("z2"), b("WEIGHTS"), b("1")];
        assert_eq!(
            keys_of("zunionstore", &args),
            vec![b("dst"), b("z1"), b("z2")]
        );
        // malformed numkeys: only dest is certain
        let args = vec![b("dst"), b("x"), b("z1")];
        assert_eq!(keys_of("zinterstore", &args), vec![b("dst")]);
    }

    #[test]
    fn blocking_commands() {
        assert!(may_block("blpop"));
        assert!(may_block("xreadgroup"));
        assert!(!may_block("lpop"));
        assert!(!may_block("xadd"));
    }
}
