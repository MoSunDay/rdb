//! E2E for the second string-command wave: INCR/DECR/INCRBY/DECRBY/
//! INCRBYFLOAT, APPEND/STRLEN/GETSET/SETNX/SETEX/PSETEX/GETDEL,
//! SETRANGE/GETRANGE. Same harness as `string_e2e.rs`: real registry,
//! slot prefixes, real RocksDB, exact RESP byte assertions.

use std::sync::{Arc, RwLock};

use rdb::{command, conf, hash, monitor, state, store, topology};

const OK: &[u8] = b"+OK\r\n";
const NOT_INT: &[u8] = b"-ERR value is not an integer or out of range\r\n";
const NOT_FLOAT: &[u8] = b"-ERR value is not a valid float\r\n";
const OVERFLOW: &[u8] = b"-ERR increment or decrement would overflow\r\n";
const NOT_FINITE: &[u8] = b"-ERR increment would produce NaN or Infinity\r\n";
const LIMIT: &[u8] = b"-ERR offset exceeds maximum allowed limit\r\n";
const SETEX_INV: &[u8] = b"-ERR invalid expire time in 'setex' command\r\n";
const PSETEX_INV: &[u8] = b"-ERR invalid expire time in 'psetex' command\r\n";
const WT: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";

/// Mirror of the lib-internal `state::testutil::shared_with`; each test
/// gets its own bind, store dir and tag.
fn shared_for(tag: &str) -> state::Shared {
    let conf = conf::Config {
        bind: format!("127.0.0.1:{tag}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:{}", tag.parse::<u16>().unwrap() + 100),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-str2-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &conf.bind);
    let st = store::open(path.to_str().unwrap()).unwrap();
    state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(&conf))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: Arc::new(rdb::lite::new_runtime()),
        sql_ts: Arc::new(rdb::sql::tx::Oracle::new()),
        migrating: Arc::new(RwLock::new(std::collections::HashMap::new())),
        importing: Arc::new(RwLock::new(std::collections::HashMap::new())),
        migrate_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        conf,
    }
}

/// Dispatch like the RESP layer on an existing runtime: real registry
/// lookup, "<slot>/" key prefixes, fresh ConnState/out per call.
async fn call_on(shared: &state::Shared, name: &str, args: &[&str]) -> Vec<u8> {
    let handler = command::lookup(name).unwrap_or_else(|| panic!("'{name}' not registered"));
    let prefix_key = if rdb::router::is_whitelisted(name) {
        Vec::new()
    } else {
        args.first()
            .map(|a| hash::slot_with_prefix(hash::hash_tag(a.as_bytes())).1)
            .unwrap_or_default()
    };
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.as_bytes().to_vec()).collect();
    let mut out = Vec::new();
    let mut conn_state = rdb::tx::session::ConnState::default();
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv,
        out: &mut out,
        close_conn: false,
        conn: &mut conn_state,
        wrote: false,
    };
    handler(&mut ctx).await;
    out
}

/// Blocking wrapper: one current-thread runtime per call.
fn call(shared: &state::Shared, name: &str, args: &[&str]) -> Vec<u8> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(call_on(shared, name, args))
}

/// Assert a command replies exactly `want`.
fn expect(shared: &state::Shared, name: &str, args: &[&str], want: &[u8]) {
    assert_eq!(call(shared, name, args), want.to_vec(), "in '{name}'");
}

/// PTTL of `key` parsed out of its `:<ms>\r\n` reply.
fn pttl_of(shared: &state::Shared, key: &str) -> i64 {
    let reply = String::from_utf8(call(shared, "pttl", &[key])).expect("ascii");
    reply
        .trim_start_matches(':')
        .trim_end()
        .parse()
        .expect("integer pttl")
}

#[test]
fn arithmetic_missing_keys_deltas_and_negatives() {
    let shared = shared_for("46101");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("incr", &["m"], b":1\r\n");
    e("incr", &["m"], b":2\r\n");
    e("decr", &["n"], b":-1\r\n");
    e("incrby", &["ib", "5"], b":5\r\n");
    e("incrby", &["ib", "-8"], b":-3\r\n"); // negative delta
    e("decrby", &["db", "3"], b":-3\r\n");
    e("decrby", &["db", "-10"], b":7\r\n"); // subtracting a negative grows
    e("set", &["ex", "10"], OK);
    e("incr", &["ex"], b":11\r\n");
    e("decr", &["ex"], b":10\r\n");
    e("incrby", &["ex", "-20"], b":-10\r\n");
    e("decrby", &["ex", "5"], b":-15\r\n");
    e("set", &["neg", "-5"], OK);
    e("decr", &["neg"], b":-6\r\n"); // DECR over a negative value
}

#[test]
fn overflow_is_refused_and_value_untouched() {
    let shared = shared_for("46102");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["max", "9223372036854775807"], OK);
    e("incr", &["max"], OVERFLOW);
    e("incrby", &["max", "1"], OVERFLOW);
    e("get", &["max"], b"$19\r\n9223372036854775807\r\n"); // unchanged
    e("incrby", &["max", "-1"], b":9223372036854775806\r\n");
    e("set", &["min", "-9223372036854775808"], OK);
    e("decr", &["min"], OVERFLOW);
    e("decrby", &["min", "1"], OVERFLOW);
    e("decrby", &["min", "-1"], b":-9223372036854775807\r\n");
    // An out-of-i64-range literal is the generic integer parse error.
    e("incrby", &["max", "9223372036854775808"], NOT_INT);
}

#[test]
fn value_errors_integer_and_float() {
    let shared = shared_for("46103");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["s", "abc"], OK);
    e("incr", &["s"], NOT_INT);
    e("decr", &["s"], NOT_INT);
    e("incrby", &["s", "1"], NOT_INT);
    e("decrby", &["s", "1"], NOT_INT);
    e("incrby", &["s", "notanumber"], NOT_INT); // bad delta precedes key read
    e("set", &["f", "1.5"], OK);
    e("incr", &["f"], NOT_INT);
    e("set", &["sp", " 5"], OK); // padded digits are not integers
    e("incr", &["sp"], NOT_INT);
    // INCRBYFLOAT: unparseable current value and delta share one text.
    e("set", &["fl", "zzz"], OK);
    e("incrbyfloat", &["fl", "1"], NOT_FLOAT);
    e("incrbyfloat", &["fl", "zzz"], NOT_FLOAT);
    // Non-finite sums (huge delta, NaN, or huge current+delta).
    e("incrbyfloat", &["nf", "1e309"], NOT_FINITE);
    e("incrbyfloat", &["nf", "nan"], NOT_FINITE);
    e("set", &["big", "1e308"], OK);
    e("incrbyfloat", &["big", "1e308"], NOT_FINITE);
    e("get", &["big"], b"$5\r\n1e308\r\n"); // refusal left the value alone
}

#[test]
fn incrbyfloat_reply_formatting_and_readback() {
    let shared = shared_for("46104");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["a", "10"], OK);
    e("incrbyfloat", &["a", "0.5"], b"$4\r\n10.5\r\n");
    e("get", &["a"], b"$4\r\n10.5\r\n"); // stored bytes equal the reply
    e("incrbyfloat", &["fresh", "3.0"], b"$1\r\n3\r\n"); // shortest roundtrip
    e("incrbyfloat", &["a", "-0.25"], b"$5\r\n10.25\r\n");
    e("set", &["neg", "1"], OK);
    e("incrbyfloat", &["neg", "-2.5"], b"$4\r\n-1.5\r\n");
}

#[test]
fn ttl_preserved_by_counters_cleared_by_setters_and_lazy_expiry() {
    let shared = shared_for("46105");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["p1", "1", "EX", "100"], OK);
    e("incr", &["p1"], b":2\r\n");
    e("set", &["p2", "1", "EX", "100"], OK);
    e("decr", &["p2"], b":0\r\n");
    e("set", &["p3", "v", "EX", "100"], OK);
    e("append", &["p3", "w"], b":2\r\n");
    e("set", &["p4", "ab", "EX", "100"], OK);
    e("setrange", &["p4", "1", "Z"], b":2\r\n");
    e("set", &["p5", "1.5", "EX", "100"], OK);
    e("incrbyfloat", &["p5", "0.5"], b"$1\r\n2\r\n");
    for key in ["p1", "p2", "p3", "p4", "p5"] {
        let ms = pttl_of(&shared, key);
        assert!(ms > 0 && ms <= 100_000, "{key} pttl {ms}");
    }
    // GETSET has plain SET semantics: the old deadline does not survive.
    e("set", &["g", "v", "EX", "100"], OK);
    e("getset", &["g", "v2"], b"$1\r\nv\r\n");
    assert_eq!(pttl_of(&shared, "g"), -1);
    // SETEX then GETDEL: value, key and expire-index entry all gone.
    e("setex", &["d", "100", "v"], OK);
    e("getdel", &["d"], b"$1\r\nv\r\n");
    e("exists", &["d"], b":0\r\n");
    e("keys", &["d*"], b"*0\r\n");
    assert_eq!(pttl_of(&shared, "d"), -2);
    // Lazy expiry: a lapsed record reads as missing, so INCR restarts at 0.
    e("set", &["lz", "5", "PX", "1"], OK);
    std::thread::sleep(std::time::Duration::from_millis(5));
    e("incr", &["lz"], b":1\r\n");
    e("get", &["lz"], b"$1\r\n1\r\n");
}

#[test]
fn wrongtype_matrix_against_a_hash_key() {
    let shared = shared_for("46106");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("hset", &["h", "f", "1"], b":1\r\n");
    for (name, args) in [
        ("incr", &["h"][..]),
        ("decr", &["h"]),
        ("incrby", &["h", "1"]),
        ("decrby", &["h", "1"]),
        ("incrbyfloat", &["h", "1"]),
        ("append", &["h", "x"]),
        ("strlen", &["h"]),
        ("getset", &["h", "x"]),
        ("getdel", &["h"]),
        ("setrange", &["h", "0", "x"]),
        ("getrange", &["h", "0", "1"]),
    ] {
        e(name, args, WT);
    }
    // Redis: SETNX's NX veto fires for ANY existing key kind (no type check).
    e("setnx", &["h", "x"], b":0\r\n");
    // The hash survived every refused read above.
    e("hget", &["h", "f"], b"$1\r\n1\r\n");
    e("hset", &["h2", "f", "1"], b":1\r\n");
    e("setex", &["h2", "100", "v"], OK);
    e("type", &["h2"], b"+string\r\n");
    e("hset", &["h3", "f", "1"], b":1\r\n");
    e("psetex", &["h3", "5000", "v"], OK);
    e("type", &["h3"], b"+string\r\n");
}

#[test]
fn append_chains_and_strlen() {
    let shared = shared_for("46107");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("append", &["a", "Hello"], b":5\r\n"); // missing key acts like SET
    e("append", &["a", " World"], b":11\r\n");
    e("get", &["a"], b"$11\r\nHello World\r\n");
    e("strlen", &["a"], b":11\r\n");
    e("strlen", &["nope"], b":0\r\n"); // missing key
    e("set", &["emp", ""], OK);
    e("strlen", &["emp"], b":0\r\n"); // empty string is a real value
    e("append", &["emp", "x"], b":1\r\n");
}

#[test]
fn setrange_padding_deletes_and_limits() {
    let shared = shared_for("46108");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("setrange", &["pad", "2", "abc"], b":5\r\n"); // zero-padded head
    e("get", &["pad"], b"$5\r\n\x00\x00abc\r\n");
    e("setrange", &["nop", "0", ""], b":0\r\n"); // empty write creates nothing
    e("exists", &["nop"], b":0\r\n");
    e("set", &["emp", ""], OK); // empty result on an existing key deletes it
    e("setrange", &["emp", "0", ""], b":0\r\n");
    e("exists", &["emp"], b":0\r\n");
    e("set", &["k", "hello"], OK);
    e("setrange", &["k", "0", "AB"], b":5\r\n");
    e("get", &["k"], b"$5\r\nABllo\r\n");
    e("setrange", &["k", "7", "z"], b":8\r\n"); // offset past the tail pads
    e("get", &["k"], b"$8\r\nABllo\x00\x00z\r\n");
    e("setrange", &["k", "536870912", "z"], LIMIT); // 2^29 cap (512MB - 1)
    e("setrange", &["k", "536870911", "z"], LIMIT); // boundary: 2^29-1 + 1 byte
    e("setrange", &["k", "-1", "z"], NOT_INT);
}

#[test]
fn getrange_indices_clamps_and_empties() {
    let shared = shared_for("46109");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["k", "hello"], OK);
    e("getrange", &["k", "0", "-1"], b"$5\r\nhello\r\n");
    e("getrange", &["k", "1", "-2"], b"$3\r\nell\r\n");
    e("getrange", &["k", "-3", "-1"], b"$3\r\nllo\r\n");
    e("getrange", &["k", "5", "10"], b"$0\r\n\r\n"); // start past the tail
    e("getrange", &["k", "-10", "1"], b"$2\r\nhe\r\n"); // start clamps to 0
    e("getrange", &["k", "2", "2"], b"$1\r\nl\r\n"); // single byte
    e("getrange", &["k", "4", "2"], b"$0\r\n\r\n"); // start > stop
    e("getrange", &["missing", "0", "-1"], b"$0\r\n\r\n");
    e("getrange", &["k", "x", "1"], NOT_INT);
}

#[test]
fn setex_psetex_expire_validation_and_deadlines() {
    let shared = shared_for("46110");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("setex", &["k", "0", "v"], SETEX_INV);
    e("setex", &["k", "-1", "v"], SETEX_INV);
    e("setex", &["k", "abc", "v"], NOT_INT);
    // i64::MAX seconds overflows the ms deadline: same invalid-expire text.
    e("setex", &["k", "9223372036854775807", "v"], SETEX_INV);
    e("psetex", &["k", "0", "v"], PSETEX_INV);
    e("psetex", &["k", "-5", "v"], PSETEX_INV);
    e("psetex", &["k", "zz", "v"], NOT_INT);
    e("setex", &["k", "100", "v"], OK);
    let ms = pttl_of(&shared, "k");
    assert!(ms > 0 && ms <= 100_000, "setex pttl {ms}");
    e("get", &["k"], b"$1\r\nv\r\n");
    e("psetex", &["p", "5000", "v"], OK);
    let ms = pttl_of(&shared, "p");
    assert!(ms > 0 && ms <= 5_000, "psetex pttl {ms}");
}

#[test]
fn getdel_returns_old_value_then_erases() {
    let shared = shared_for("46111");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("getdel", &["none"], b"$-1\r\n");
    e("set", &["k", "val"], OK);
    e("getdel", &["k"], b"$3\r\nval\r\n");
    e("get", &["k"], b"$-1\r\n");
    e("type", &["k"], b"+none\r\n");
    e("exists", &["k"], b":0\r\n");
}

#[test]
fn arity_errors_quote_the_command_name() {
    let shared = shared_for("46112");
    let cases: &[(&str, &[&str])] = &[
        ("incr", &["a", "b"]),
        ("incrby", &["a"]),
        ("decrby", &["a"]),
        ("incrbyfloat", &["a"]),
        ("append", &["a"]),
        ("strlen", &["a", "b"]),
        ("getset", &["a"]),
        ("setnx", &["a"]),
        ("setex", &["a", "1"]),
        ("psetex", &["a", "1"]),
        ("getdel", &["a", "b"]),
        ("setrange", &["a", "0"]),
        ("getrange", &["a", "0"]),
    ];
    for (name, args) in cases {
        let want = format!("-ERR wrong number of arguments for '{name}' command\r\n");
        expect(&shared, name, args, want.as_bytes());
    }
}

#[test]
fn mixed_family_value_coupling() {
    let shared = shared_for("46113");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // SETEX writes a number, APPEND corrupts it, INCR sees the coupled
    // bytes; the SETEX deadline still survives the APPEND.
    e("setex", &["k1", "100", "1"], OK);
    e("append", &["k1", "x"], b":2\r\n");
    e("get", &["k1"], b"$2\r\n1x\r\n");
    e("incr", &["k1"], NOT_INT);
    let ms = pttl_of(&shared, "k1");
    assert!(ms > 0 && ms <= 100_000, "k1 pttl {ms}");
    // GETSET on a missing key still writes (and returns null).
    e("getset", &["nokey", "v"], b"$-1\r\n");
    e("exists", &["nokey"], b":1\r\n");
    // SETNX: creates once, refuses the second write, keeps the first value.
    e("setnx", &["nx", "a"], b":1\r\n");
    e("setnx", &["nx", "b"], b":0\r\n");
    e("get", &["nx"], b"$1\r\na\r\n");
}

/// 32 tasks x 25 INCRs through the registry on one multi-thread runtime:
/// the per-key latch must serialize every read-modify-write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_incrs_serialize_to_exactly_800() {
    let shared = Arc::new(shared_for("46120"));
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let s = Arc::clone(&shared);
        tasks.push(tokio::spawn(async move {
            for _ in 0..25 {
                let reply = call_on(&s, "incr", &["c"]).await;
                assert!(
                    reply.starts_with(b":") && reply.ends_with(b"\r\n"),
                    "{reply:?}"
                );
            }
        }));
    }
    for t in tasks {
        t.await.expect("incr worker");
    }
    assert_eq!(
        call_on(&shared, "get", &["c"]).await,
        b"$3\r\n800\r\n".to_vec()
    );
}
