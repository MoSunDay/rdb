//! The ALLOWED-surface table: one row per entry of
//! `src/command/readonly.rs::ALLOWED` (87 commands + a free second
//! XINFO variant), with the expected-reply predicate machinery. All
//! data reads pin MISSING-KEY shapes (the backup store is separate and
//! empty); state/conn commands pin their real replies.

use crate::common::contains_bytes;

use super::GATE;

/// Does this row need the FULL reply frame (nested arrays are not
/// line-resolvable through `cmd_one_shot`)?
pub fn needs_full(p: &Pred) -> bool {
    matches!(p, Pred::Frame(_))
}

/// Expected-reply predicate for one table row.
pub enum Pred {
    /// Byte-exact single-line reply (cmd_one_shot, CRLF stripped).
    Exact(&'static [u8]),
    /// Byte-exact FULL frame incl. CRLFs (cmd_full_reply; arrays).
    Frame(&'static [u8]),
    /// Reply starts with this RESP type byte (+ : $ *).
    Starts(u8),
    /// The command ran: any non-error reply (never the gate error).
    Runs,
    /// The command's OWN error (gate passed, handler refused on its own
    /// terms): an error line containing this text.
    OwnErr(&'static str),
}

/// One ALLOWED command to prove readable through the backup listener.
pub type Row = (&'static str, &'static [&'static [u8]], Pred);

fn describe(p: &Pred) -> String {
    match p {
        Pred::Exact(w) | Pred::Frame(w) => format!("exact {:?}", String::from_utf8_lossy(w)),
        Pred::Starts(c) => format!("starts with '{}'", *c as char),
        Pred::Runs => "any non-error reply".to_string(),
        Pred::OwnErr(t) => format!("own error containing {t:?}"),
    }
}

/// Assert one reply against its predicate; every failure names the command
/// and NEVER tolerates the gate error (an ALLOWED command must never hit it).
pub fn check(name: &str, reply: &[u8], p: &Pred, ctx: &str) {
    assert!(
        !contains_bytes(reply, &GATE[1..]),
        "{name} hit the READONLY gate: {:?}\n{ctx}",
        String::from_utf8_lossy(reply)
    );
    let ok = match p {
        Pred::Exact(w) => reply == *w,
        Pred::Frame(w) => reply == *w,
        Pred::Starts(c) => reply.first() == Some(c),
        Pred::Runs => !reply.starts_with(b"-"),
        Pred::OwnErr(t) => reply.starts_with(b"-") && contains_bytes(reply, t.as_bytes()),
    };
    assert!(
        ok,
        "{name}: expected {}, got {:?}\n{ctx}",
        describe(p),
        String::from_utf8_lossy(reply)
    );
}

/// The full ALLOWED surface: every entry of readonly.rs::ALLOWED, one row
/// each (XINFO gets a second, non-error variant row for free).
pub fn table() -> Vec<Row> {
    vec![
        // -- protocol / meta --
        ("ping", &[b"PING"], Pred::Exact(b"+PONG")),
        ("quit", &[b"QUIT"], Pred::Exact(b"+OK")),
        ("echo", &[b"ECHO", b"hi"], Pred::Exact(b"$2\r\nhi")),
        ("select", &[b"SELECT", b"0"], Pred::Exact(b"+OK")),
        ("command", &[b"COMMAND"], Pred::Starts(b'*')),
        ("info", &[b"INFO"], Pred::Starts(b'$')),
        ("dbsize", &[b"DBSIZE"], Pred::Exact(b":0")),
        (
            "config",
            &[b"CONFIG", b"GET", b"cluster-require-full-coverage"],
            Pred::Frame(b"*2\r\n$29\r\ncluster-require-full-coverage\r\n$2\r\nno\r\n"),
        ),
        ("asking", &[b"ASKING"], Pred::Exact(b"+OK")),
        // -- string reads --
        ("get", &[b"GET", b"sk"], Pred::Exact(b"$-1")),
        (
            "mget",
            &[b"MGET", b"{b}k1", b"{b}k2"],
            Pred::Frame(b"*2\r\n$-1\r\n$-1\r\n"),
        ),
        ("strlen", &[b"STRLEN", b"sk"], Pred::Exact(b":0")),
        (
            "getrange",
            &[b"GETRANGE", b"sk", b"0", b"1"],
            Pred::Exact(b"$0\r\n"),
        ),
        ("getbit", &[b"GETBIT", b"sk", b"0"], Pred::Exact(b":0")),
        ("bitcount", &[b"BITCOUNT", b"sk"], Pred::Exact(b":0")),
        ("bitpos", &[b"BITPOS", b"sk", b"0"], Pred::Exact(b":0")),
        // -- key reads --
        ("exists", &[b"EXISTS", b"sk"], Pred::Exact(b":0")),
        ("type", &[b"TYPE", b"sk"], Pred::Exact(b"+none")),
        ("ttl", &[b"TTL", b"sk"], Pred::Exact(b":-2")),
        ("pttl", &[b"PTTL", b"sk"], Pred::Exact(b":-2")),
        ("scan", &[b"SCAN", b"0"], Pred::Starts(b'*')),
        ("keys", &[b"KEYS", b"*"], Pred::Exact(b"*0")),
        ("randomkey", &[b"RANDOMKEY"], Pred::Exact(b"$-1")),
        // -- hash reads --
        ("hget", &[b"HGET", b"hk", b"f1"], Pred::Exact(b"$-1")),
        (
            "hmget",
            &[b"HMGET", b"hk", b"f1", b"f2"],
            Pred::Frame(b"*2\r\n$-1\r\n$-1\r\n"),
        ),
        ("hlen", &[b"HLEN", b"hk"], Pred::Exact(b":0")),
        ("hexists", &[b"HEXISTS", b"hk", b"f1"], Pred::Exact(b":0")),
        ("hstrlen", &[b"HSTRLEN", b"hk", b"f1"], Pred::Exact(b":0")),
        ("hgetall", &[b"HGETALL", b"hk"], Pred::Exact(b"*0")),
        ("hkeys", &[b"HKEYS", b"hk"], Pred::Exact(b"*0")),
        ("hvals", &[b"HVALS", b"hk"], Pred::Exact(b"*0")),
        ("hrandfield", &[b"HRANDFIELD", b"hk"], Pred::Exact(b"$-1")),
        ("hscan", &[b"HSCAN", b"hk", b"0"], Pred::Starts(b'*')),
        // -- set reads --
        ("smembers", &[b"SMEMBERS", b"setk"], Pred::Exact(b"*0")),
        (
            "sismember",
            &[b"SISMEMBER", b"setk", b"m1"],
            Pred::Exact(b":0"),
        ),
        (
            "smismember",
            &[b"SMISMEMBER", b"setk", b"m1", b"m2"],
            Pred::Frame(b"*2\r\n:0\r\n:0\r\n"),
        ),
        ("scard", &[b"SCARD", b"setk"], Pred::Exact(b":0")),
        (
            "srandmember",
            &[b"SRANDMEMBER", b"setk"],
            Pred::Exact(b"$-1"),
        ),
        ("sscan", &[b"SSCAN", b"setk", b"0"], Pred::Starts(b'*')),
        ("sdiff", &[b"SDIFF", b"{s}sa", b"{s}sb"], Pred::Exact(b"*0")),
        (
            "sinter",
            &[b"SINTER", b"{s}sa", b"{s}sb"],
            Pred::Exact(b"*0"),
        ),
        (
            "sunion",
            &[b"SUNION", b"{s}sa", b"{s}sb"],
            Pred::Exact(b"*0"),
        ),
        (
            "sintercard",
            &[b"SINTERCARD", b"2", b"{s}sa", b"{s}sb"],
            Pred::Exact(b":0"),
        ),
        // -- list reads --
        ("lindex", &[b"LINDEX", b"lk", b"0"], Pred::Exact(b"$-1")),
        ("llen", &[b"LLEN", b"lk"], Pred::Exact(b":0")),
        (
            "lrange",
            &[b"LRANGE", b"lk", b"0", b"-1"],
            Pred::Exact(b"*0"),
        ),
        ("lpos", &[b"LPOS", b"lk", b"a"], Pred::Exact(b"$-1")),
        // -- zset reads --
        ("zcard", &[b"ZCARD", b"zsk"], Pred::Exact(b":0")),
        ("zscore", &[b"ZSCORE", b"zsk", b"a"], Pred::Exact(b"$-1")),
        (
            "zmscore",
            &[b"ZMSCORE", b"zsk", b"a"],
            Pred::Frame(b"*1\r\n$-1\r\n"),
        ),
        (
            "zcount",
            &[b"ZCOUNT", b"zsk", b"1", b"2"],
            Pred::Exact(b":0"),
        ),
        ("zrank", &[b"ZRANK", b"zsk", b"a"], Pred::Exact(b"$-1")),
        (
            "zrevrank",
            &[b"ZREVRANK", b"zsk", b"a"],
            Pred::Exact(b"$-1"),
        ),
        (
            "zrandmember",
            &[b"ZRANDMEMBER", b"zsk"],
            Pred::Exact(b"$-1"),
        ),
        (
            "zrange",
            &[b"ZRANGE", b"zsk", b"0", b"-1"],
            Pred::Exact(b"*0"),
        ),
        (
            "zrevrange",
            &[b"ZREVRANGE", b"zsk", b"0", b"-1"],
            Pred::Exact(b"*0"),
        ),
        (
            "zrangebyscore",
            &[b"ZRANGEBYSCORE", b"zsk", b"1", b"2"],
            Pred::Exact(b"*0"),
        ),
        (
            "zrevrangebyscore",
            &[b"ZREVRANGEBYSCORE", b"zsk", b"2", b"1"],
            Pred::Exact(b"*0"),
        ),
        (
            "zrangebylex",
            &[b"ZRANGEBYLEX", b"zsk", b"-", b"+"],
            Pred::Exact(b"*0"),
        ),
        (
            "zrevrangebylex",
            &[b"ZREVRANGEBYLEX", b"zsk", b"+", b"-"],
            Pred::Exact(b"*0"),
        ),
        (
            "zlexcount",
            &[b"ZLEXCOUNT", b"zsk", b"-", b"+"],
            Pred::Exact(b":0"),
        ),
        ("zscan", &[b"ZSCAN", b"zsk", b"0"], Pred::Starts(b'*')),
        // -- stream reads --
        ("xlen", &[b"XLEN", b"st/q1"], Pred::Exact(b":0")),
        (
            "xrange",
            &[b"XRANGE", b"st/q1", b"-", b"+"],
            Pred::Exact(b"*0"),
        ),
        (
            "xrevrange",
            &[b"XREVRANGE", b"st/q1", b"+", b"-"],
            Pred::Exact(b"*0"),
        ),
        (
            "xinfo",
            &[b"XINFO", b"STREAM", b"st/q1"],
            Pred::OwnErr("no such key"),
        ),
        (
            "xinfo",
            &[b"XINFO", b"GROUPS", b"st/q1"],
            Pred::Exact(b"*0"),
        ),
        (
            "xpending",
            &[b"XPENDING", b"st/q1", b"g1"],
            Pred::Starts(b'-'),
        ),
        (
            "xread",
            &[b"XREAD", b"COUNT", b"10", b"STREAMS", b"st/q1", b"0-0"],
            Pred::Starts(b'*'),
        ),
        // -- vector-set reads --
        ("vcard", &[b"VCARD", b"vk"], Pred::Exact(b":0")),
        (
            "vdim",
            &[b"VDIM", b"vk"],
            Pred::OwnErr("vector set does not exist"),
        ),
        (
            "vgetattr",
            &[b"VGETATTR", b"vk", b"e1"],
            Pred::Exact(b"$-1"),
        ),
        (
            "vsim",
            &[b"VSIM", b"vk"],
            Pred::OwnErr("vector set does not exist"),
        ),
        // -- json reads --
        ("json.get", &[b"JSON.GET", b"jk"], Pred::Exact(b"$-1")),
        (
            "json.mget",
            &[b"JSON.MGET", b"jk", b"$"],
            Pred::Frame(b"*1\r\n$-1\r\n"),
        ),
        ("json.type", &[b"JSON.TYPE", b"jk"], Pred::Exact(b"$-1")),
        ("json.strlen", &[b"JSON.STRLEN", b"jk"], Pred::Exact(b"$-1")),
        (
            "json.arrindex",
            &[b"JSON.ARRINDEX", b"jk", b"$.b.c", b"1"],
            Pred::Exact(b":-1"),
        ),
        (
            "json.arrlen",
            &[b"JSON.ARRLEN", b"jk", b"$.b.c"],
            Pred::Exact(b"$-1"),
        ),
        (
            "json.objkeys",
            &[b"JSON.OBJKEYS", b"jk"],
            Pred::Exact(b"$-1"),
        ),
        ("json.objlen", &[b"JSON.OBJLEN", b"jk"], Pred::Exact(b"$-1")),
        // -- search reads --
        (
            "ft.info",
            &[b"FT.INFO", b"fidx"],
            Pred::OwnErr("unknown index"),
        ),
        (
            "ft.search",
            &[b"FT.SEARCH", b"fidx", b"seed"],
            Pred::OwnErr("unknown index"),
        ),
        // -- transaction controls --
        ("multi", &[b"MULTI"], Pred::Exact(b"+OK")),
        ("exec", &[b"EXEC"], Pred::OwnErr("EXEC without MULTI")),
        (
            "discard",
            &[b"DISCARD"],
            Pred::OwnErr("DISCARD without MULTI"),
        ),
        ("watch", &[b"WATCH", b"sk"], Pred::Exact(b"+OK")),
        ("unwatch", &[b"UNWATCH"], Pred::Exact(b"+OK")),
    ]
}
