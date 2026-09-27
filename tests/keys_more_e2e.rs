//! Generic key-space command e2e, round 2: in-process registry with exact
//! reply bytes for UNLINK, EXPIREAT/PEXPIREAT (past deadline = immediate
//! delete), PERSIST, RANDOMKEY (deterministic while keys sit in one slot),
//! RENAMENX, TYPE per family, KEYS glob patterns + lazy-TTL visibility,
//! and the fixed CONFIG stub reply.

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use common::lite::{call, shared_at, text};

fn arity_err(cmd: &str) -> Vec<u8> {
    format!("-ERR wrong number of arguments for '{cmd}' command\r\n").into_bytes()
}

/// Flat bulk array reply of KEYS.
fn arr(items: &[&[u8]]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", items.len()).into_bytes();
    for i in items {
        buf.extend_from_slice(format!("${}\r\n", i.len()).as_bytes());
        buf.extend_from_slice(i);
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

/// TTL/PTTL reply parsed to an integer.
fn int_reply(reply: &[u8], what: &str) -> i64 {
    let t = text(reply);
    let n = t
        .trim_end_matches("\r\n")
        .strip_prefix(':')
        .unwrap_or_else(|| panic!("{what}: {t:?}"));
    n.parse().unwrap_or_else(|_| panic!("{what}: {t:?}"))
}

#[test]
fn unlink_counts_and_removes_both_shapes() {
    let (shared, _dir) = shared_at("45401");
    let e = |name: &str, args: &[&[u8]], want: &[u8]| {
        assert_eq!(call(&shared, name, args), want.to_vec(), "in '{name}'");
    };
    // UNLINK is dispatched onto the DEL handler: the arity error (and the
    // storage-error text) carries the canonical 'del' name.
    e(
        "unlink",
        &[],
        b"-ERR wrong number of arguments for 'del' command\r\n",
    );
    e("set", &[b"{u}a", b"1"], b"+OK\r\n");
    e("set", &[b"{u}b", b"2"], b"+OK\r\n");
    e("unlink", &[b"{u}a", b"{u}b"], b":2\r\n");
    e("get", &[b"{u}a"], b"$-1\r\n");
    e("unlink", &[b"{u}a"], b":0\r\n");
    // Typed family: one count, the whole record family goes with it.
    e("hset", &[b"{u}h", b"f", b"v"], b":1\r\n");
    e("unlink", &[b"{u}h"], b":1\r\n");
    e("hgetall", &[b"{u}h"], b"*0\r\n");
    // Keys inside one UNLINK must share a slot.
    e(
        "unlink",
        &[b"a", b"b"],
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n",
    );
}

#[test]
fn expireat_pexpireat_past_dies_future_sets_ttl() {
    let (shared, _dir) = shared_at("45402");
    let e = |name: &str, args: &[&[u8]], want: &[u8]| {
        assert_eq!(call(&shared, name, args), want.to_vec(), "in '{name}'");
    };
    // A timestamp in the past clamps to 0 = delete: the key dies at once.
    e("set", &[b"{e}k", b"v"], b"+OK\r\n");
    e("expireat", &[b"{e}k", b"1"], b":1\r\n");
    e("get", &[b"{e}k"], b"$-1\r\n");
    e("set", &[b"{e}p", b"v"], b"+OK\r\n");
    e("pexpireat", &[b"{e}p", b"1"], b":1\r\n");
    e("exists", &[b"{e}p"], b":0\r\n");
    // Missing key: nothing to change.
    e("expireat", &[b"{e}none", b"9999999999"], b":0\r\n");
    // Far-future absolute deadlines (now_ms is wall clock; bounds only).
    const FAR_S: i64 = 4_102_444_800; // 2100-01-01 UTC
    e("set", &[b"{e}f", b"v"], b"+OK\r\n");
    e(
        "expireat",
        &[b"{e}f", FAR_S.to_string().as_bytes()],
        b":1\r\n",
    );
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let secs = int_reply(&call(&shared, "ttl", &[b"{e}f"]), "ttl");
    assert!(
        secs >= FAR_S - now - 60 && secs <= FAR_S - now + 60,
        "ttl {secs}"
    );
    // PEXPIREAT is milliseconds-absolute.
    let ms = (rdb::ds::expire::now_ms() + 300_000).to_string();
    e("set", &[b"{e}m", b"v"], b"+OK\r\n");
    e("pexpireat", &[b"{e}m", ms.as_bytes()], b":1\r\n");
    let left = int_reply(&call(&shared, "pttl", &[b"{e}m"]), "pttl");
    assert!(left > 0 && left <= 300_000, "pttl {left}");
    // Arity, flag and integer parsing.
    e("expireat", &[b"{e}f"], &arity_err("expireat"));
    e(
        "expireat",
        &[b"{e}f", b"1", b"FOO"],
        b"-ERR Unsupported option: supported options are NX, XX, GT and LT\r\n",
    );
    e(
        "pexpireat",
        &[b"{e}f", b"abc"],
        b"-ERR value is not an integer or out of range\r\n",
    );
}

#[test]
fn persist_clears_ttl_only_when_one_exists() {
    let (shared, _dir) = shared_at("45403");
    let e = |name: &str, args: &[&[u8]], want: &[u8]| {
        assert_eq!(call(&shared, name, args), want.to_vec(), "in '{name}'");
    };
    e("persist", &[], &arity_err("persist"));
    e("persist", &[b"{p}none"], b":0\r\n");
    // No TTL to clear.
    e("set", &[b"{p}a", b"v"], b"+OK\r\n");
    e("persist", &[b"{p}a"], b":0\r\n");
    // Enveloped string migrates back to a bare record, value intact.
    e("set", &[b"{p}b", b"v"], b"+OK\r\n");
    e("expire", &[b"{p}b", b"100"], b":1\r\n");
    e("persist", &[b"{p}b"], b":1\r\n");
    e("ttl", &[b"{p}b"], b":-1\r\n");
    e("get", &[b"{p}b"], b"$1\r\nv\r\n");
    e("persist", &[b"{p}b"], b":0\r\n");
    // Typed family: TTL drops, elements stay.
    e("rpush", &[b"{p}l", b"a", b"b"], b":2\r\n");
    e("expire", &[b"{p}l", b"100"], b":1\r\n");
    e("persist", &[b"{p}l"], b":1\r\n");
    e("llen", &[b"{p}l"], b":2\r\n");
}

#[test]
fn randomkey_null_when_empty_then_first_single_slot_key() {
    let (shared, _dir) = shared_at("45404");
    // Empty keyspace: null bulk.
    assert_eq!(call(&shared, "randomkey", &[]), b"$-1\r\n".to_vec());
    assert_eq!(call(&shared, "randomkey", &[b"x"]), arity_err("randomkey"));
    // RANDOMKEY is keyless (whitelisted): it starts at a uniformly random
    // slot and wraps to the database start. With every key inside ONE
    // slot any start lands on the same first physical key, so the reply
    // is deterministic: the lexicographically first user key.
    for k in ["{r}c", "{r}a", "{r}b"] {
        assert_eq!(
            call(&shared, "set", &[k.as_bytes(), b"v"]),
            b"+OK\r\n".to_vec()
        );
    }
    for _ in 0..8 {
        assert_eq!(call(&shared, "randomkey", &[]), b"$4\r\n{r}a\r\n".to_vec());
    }
}

#[test]
fn renamenx_moves_blocks_and_reports_missing_source() {
    let (shared, _dir) = shared_at("45405");
    let e = |name: &str, args: &[&[u8]], want: &[u8]| {
        assert_eq!(call(&shared, name, args), want.to_vec(), "in '{name}'");
    };
    e("renamenx", &[b"{n}src"], &arity_err("renamenx"));
    // Free destination: the move happens.
    e("set", &[b"{n}src", b"v"], b"+OK\r\n");
    e("renamenx", &[b"{n}src", b"{n}dst"], b":1\r\n");
    e("exists", &[b"{n}src"], b":0\r\n");
    e("get", &[b"{n}dst"], b"$1\r\nv\r\n");
    // Existing destination blocks the move, both values untouched.
    e("set", &[b"{n}s2", b"v"], b"+OK\r\n");
    e("set", &[b"{n}d2", b"w"], b"+OK\r\n");
    e("renamenx", &[b"{n}s2", b"{n}d2"], b":0\r\n");
    e("get", &[b"{n}s2"], b"$1\r\nv\r\n");
    e("get", &[b"{n}d2"], b"$1\r\nw\r\n");
    // Missing source is an error, not :0.
    e("renamenx", &[b"{n}nope", b"{n}x"], b"-ERR no such key\r\n");
    // A typed family moves its records with the key.
    e("hset", &[b"{n}h1", b"f", b"v"], b":1\r\n");
    e("renamenx", &[b"{n}h1", b"{n}h2"], b":1\r\n");
    e("hget", &[b"{n}h2", b"f"], b"$1\r\nv\r\n");
    e("exists", &[b"{n}h1"], b":0\r\n");
}

#[test]
fn type_names_each_family_and_none() {
    let (shared, _dir) = shared_at("45406");
    let e = |name: &str, args: &[&[u8]], want: &[u8]| {
        assert_eq!(call(&shared, name, args), want.to_vec(), "in '{name}'");
    };
    e("type", &[], &arity_err("type"));
    e("type", &[b"k", b"extra"], &arity_err("type"));
    e("type", &[b"missing"], b"+none\r\n");
    e("set", &[b"{t}s", b"v"], b"+OK\r\n");
    e("type", &[b"{t}s"], b"+string\r\n");
    // A TTL'd string is still "string".
    e("expire", &[b"{t}s", b"100"], b":1\r\n");
    e("type", &[b"{t}s"], b"+string\r\n");
    e("hset", &[b"{t}h", b"f", b"v"], b":1\r\n");
    e("type", &[b"{t}h"], b"+hash\r\n");
    e("sadd", &[b"{t}set", b"m"], b":1\r\n");
    e("type", &[b"{t}set"], b"+set\r\n");
    e("rpush", &[b"{t}l", b"e"], b":1\r\n");
    e("type", &[b"{t}l"], b"+list\r\n");
    e("zadd", &[b"{t}z", b"1", b"m"], b":1\r\n");
    e("type", &[b"{t}z"], b"+zset\r\n");
}

#[test]
fn keys_glob_patterns_and_lazy_expiry_visibility() {
    let (shared, _dir) = shared_at("45407");
    let e = |name: &str, args: &[&[u8]], want: &[u8]| {
        assert_eq!(call(&shared, name, args), want.to_vec(), "in '{name}'");
    };
    e("keys", &[], &arity_err("keys"));
    e("keys", &[b"*", b"extra"], &arity_err("keys"));
    for k in [b"{k}a".as_slice(), b"{k}ab", b"{k}b", b"{k}c1"] {
        e("set", &[k, b"v"], b"+OK\r\n");
    }
    // Same slot, all bare records: physical order == user key byte order.
    e(
        "keys",
        &[b"*"],
        &arr(&[b"{k}a", b"{k}ab", b"{k}b", b"{k}c1"]),
    );
    // '?' eats exactly one byte, classes one byte of a set/range.
    e("keys", &[b"{k}?"], &arr(&[b"{k}a", b"{k}b"]));
    e("keys", &[b"{k}[ab]*"], &arr(&[b"{k}a", b"{k}ab", b"{k}b"]));
    e("keys", &[b"{k}c?"], &arr(&[b"{k}c1"]));
    e("keys", &[b"zz*"], b"*0\r\n");
    // Lazy expiry: a due record stays listed (KEYS never resolves TTLs);
    // the enveloped record sorts BEFORE the bare strings of the slot.
    e("set", &[b"{k}dead", b"v", b"PXAT", b"1"], b"+OK\r\n");
    e(
        "keys",
        &[b"*"],
        &arr(&[b"{k}dead", b"{k}a", b"{k}ab", b"{k}b", b"{k}c1"]),
    );
    // A resolving read (TYPE) purges it synchronously.
    e("type", &[b"{k}dead"], b"+none\r\n");
    e(
        "keys",
        &[b"*"],
        &arr(&[b"{k}a", b"{k}ab", b"{k}b", b"{k}c1"]),
    );
}

#[test]
fn config_stub_replies_fixed_pair_for_any_args() {
    let (shared, _dir) = shared_at("45408");
    // CONFIG is a stub: no arity or subcommand validation, always the
    // same 2-element array (29 = len("cluster-require-full-coverage")).
    let fixed: &[u8] = b"*2\r\n$29\r\ncluster-require-full-coverage\r\n$2\r\nno\r\n";
    assert_eq!(call(&shared, "config", &[]), fixed.to_vec());
    assert_eq!(call(&shared, "config", &[b"get", b"*"]), fixed.to_vec());
    assert_eq!(
        call(&shared, "config", &[b"set", b"maxmemory", b"100"]),
        fixed.to_vec()
    );
    assert_eq!(call(&shared, "config", &[b"anything"]), fixed.to_vec());
}
