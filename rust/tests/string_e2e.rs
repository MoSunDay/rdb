//! End-to-end tests for the string family: dispatch through the real
//! command registry with slot-derived prefixes (mirroring the RESP
//! layer) against a real RocksDB store. Covers the full SET option
//! matrix (NX/XX/GET/KEEPTTL/EX/PX/EXAT/PXAT, syntax errors, option
//! case-insensitivity) plus MSET/MGET and the CROSSSLOT rule for
//! multi-key string commands. `expect` asserts exact RESP reply bytes.

use std::sync::{Arc, RwLock};

use rdb::{command, conf, hash, monitor, state, store, topology};

/// Mirror of `state::testutil::shared_with` (lib-internal, invisible
/// here); each test gets its own bind, store dir and tag.
fn shared_for(tag: &str) -> state::Shared {
    let conf = conf::Config {
        bind: format!("127.0.0.1:{tag}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:{}", tag.parse::<u16>().unwrap() + 100),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-str-e2e-{}-{tag}", std::process::id()));
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
        conf,
    }
}

/// Dispatch like the RESP layer: registry lookup, slot prefix from the
/// first key arg (empty for whitelisted/keyless commands), one
/// current-thread runtime per call. Returns the reply bytes.
fn call(shared: &state::Shared, name: &str, args: &[&str]) -> Vec<u8> {
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
    let mut ctx = command::Ctx {
        shared,
        prefix_key,
        args: argv,
        out: &mut out,
        close_conn: false,
    };
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(handler(&mut ctx));
    out
}

/// Assert a command replies exactly `want`.
fn expect(shared: &state::Shared, name: &str, args: &[&str], want: &[u8]) {
    assert_eq!(call(shared, name, args), want.to_vec(), "in '{name}'");
}

/// PTTL of `key` parsed out of its `:<ms>\r\n` reply.
fn pttl_ms(shared: &state::Shared, key: &str) -> i64 {
    let reply = String::from_utf8(call(shared, "pttl", &[key])).expect("ascii");
    reply
        .trim_start_matches(':')
        .trim_end()
        .parse()
        .expect("integer pttl")
}

#[test]
fn plain_set_get_overwrite_and_delete() {
    let shared = shared_for("46001");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["k", "v"], b"+OK\r\n");
    e("get", &["k"], b"$1\r\nv\r\n");
    e("type", &["k"], b"+string\r\n");
    // Overwrite in place; empty values are real values.
    e("set", &["k", "longer"], b"+OK\r\n");
    e("get", &["k"], b"$6\r\nlonger\r\n");
    e("set", &["k", ""], b"+OK\r\n");
    e("get", &["k"], b"$0\r\n\r\n");
    e("get", &["missing"], b"$-1\r\n");
    e("del", &["k"], b":1\r\n");
    e("get", &["k"], b"$-1\r\n");
}

#[test]
fn set_nx_xx_vetoes() {
    let shared = shared_for("46002");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // NX on a missing key writes; on an existing key it vetoes (null).
    e("set", &["fresh", "x", "NX"], b"+OK\r\n");
    e("set", &["fresh", "y", "NX"], b"$-1\r\n");
    e("get", &["fresh"], b"$1\r\nx\r\n");
    // XX on a missing key vetoes; on an existing key it writes.
    e("set", &["ghost", "y", "XX"], b"$-1\r\n");
    e("get", &["ghost"], b"$-1\r\n");
    e("set", &["fresh", "z", "XX"], b"+OK\r\n");
    e("get", &["fresh"], b"$1\r\nz\r\n");
}

#[test]
fn set_get_option_old_value_or_null() {
    let shared = shared_for("46003");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // Success + GET replies the OLD value; a first write replies null.
    e("set", &["g", "new", "GET"], b"$-1\r\n");
    e("get", &["g"], b"$3\r\nnew\r\n");
    e("set", &["g", "newer", "GET"], b"$3\r\nnew\r\n");
    e("get", &["g"], b"$5\r\nnewer\r\n");
    // NX+GET veto: the write is refused but the old value is reported.
    e("set", &["g", "nope", "NX", "GET"], b"$5\r\nnewer\r\n");
    e("get", &["g"], b"$5\r\nnewer\r\n");
    // XX+GET on a missing key: null veto, nothing written.
    e("set", &["none", "v", "XX", "GET"], b"$-1\r\n");
    e("exists", &["none"], b":0\r\n");
    // NX+GET on a missing key succeeds and reports null.
    e("set", &["none", "v", "NX", "GET"], b"$-1\r\n");
    e("get", &["none"], b"$1\r\nv\r\n");
    // GET against a non-string key refuses the whole command.
    e("hset", &["h", "f", "1"], b":1\r\n");
    e(
        "set",
        &["h", "v", "GET"],
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    );
}

#[test]
fn set_ttl_options_write_deadlines() {
    let shared = shared_for("46004");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["k", "v", "EX", "100"], b"+OK\r\n");
    let ms = pttl_ms(&shared, "k");
    assert!(ms > 0 && ms <= 100_000, "ex pttl {ms}");
    e("set", &["k", "v", "PX", "50000"], b"+OK\r\n");
    let ms = pttl_ms(&shared, "k");
    assert!(ms > 0 && ms <= 50_000, "px pttl {ms}");
    // EXAT seconds / PXAT milliseconds are absolute deadlines.
    let exat = (rdb::ds::expire::now_ms() / 1000 + 60).to_string();
    e("set", &["k", "v", "EXAT", &exat], b"+OK\r\n");
    let ms = pttl_ms(&shared, "k");
    assert!(ms > 0 && ms <= 60_000, "exat pttl {ms}");
    let pxat = (rdb::ds::expire::now_ms() + 30_000).to_string();
    e("set", &["k", "v", "PXAT", &pxat], b"+OK\r\n");
    let ms = pttl_ms(&shared, "k");
    assert!(ms > 0 && ms <= 30_000, "pxat pttl {ms}");
    // KEEPTTL replaces the value but carries the deadline over ...
    e("set", &["k", "kept", "KEEPTTL"], b"+OK\r\n");
    e("get", &["k"], b"$4\r\nkept\r\n");
    let ms = pttl_ms(&shared, "k");
    assert!(ms > 0 && ms <= 30_000, "keepttl pttl {ms}");
    // ... while a plain SET without KEEPTTL drops the deadline.
    e("set", &["k", "plain"], b"+OK\r\n");
    assert_eq!(pttl_ms(&shared, "k"), -1);
}

#[test]
fn set_short_ttl_expires_and_past_exat_still_writes() {
    let shared = shared_for("46005");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // A short real PX: the value vanishes after the deadline.
    e("set", &["soon", "v", "PX", "150"], b"+OK\r\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    e("get", &["soon"], b"$-1\r\n");
    e("ttl", &["soon"], b":-2\r\n");
    // A past EXAT deadline still WRITES the record (this impl matches
    // Redis): +OK now, gone on the next lazy-expiring read.
    e("set", &["past", "gone", "EXAT", "1"], b"+OK\r\n");
    e("get", &["past"], b"$-1\r\n");
    e("exists", &["past"], b":0\r\n");
}

#[test]
fn set_syntax_and_expire_errors() {
    let shared = shared_for("46006");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // Missing value: plain arity error naming the command.
    e(
        "set",
        &["k"],
        b"-ERR wrong number of arguments for 'set' command\r\n",
    );
    // Unknown option / EX without its value / NX+XX / repeated GET /
    // KEEPTTL mixed with a TTL option: all plain syntax errors.
    for args in [
        &["k", "v", "WHAT"][..],
        &["k", "v", "EX"][..],
        &["k", "v", "NX", "XX"][..],
        &["k", "v", "GET", "GET"][..],
        &["k", "v", "KEEPTTL", "EX", "10"][..],
        &["k", "v", "EX", "10", "KEEPTTL"][..],
    ] {
        e("set", args, b"-ERR syntax error\r\n");
    }
    // Non-positive TTL values carry the dedicated 'set' message.
    for arg in ["0", "-5", "-1"] {
        e(
            "set",
            &["k", "v", "EX", arg],
            b"-ERR invalid expire time in 'set' command\r\n",
        );
    }
    e(
        "set",
        &["k", "v", "PX", "0"],
        b"-ERR invalid expire time in 'set' command\r\n",
    );
    // Unknown option spelled like a TTL verb ("P") is a plain syntax error.
    e("set", &["k", "v", "P", "1.5"], b"-ERR syntax error\r\n");
    // A non-integer TTL argument is the generic integer error.
    e(
        "set",
        &["k", "v", "EX", "abc"],
        b"-ERR value is not an integer or out of range\r\n",
    );
    e(
        "set",
        &["k", "v", "PXAT", "1.5"],
        b"-ERR value is not an integer or out of range\r\n",
    );
    // Nothing was written along the way.
    e("get", &["k"], b"$-1\r\n");
}

#[test]
fn set_options_are_case_insensitive() {
    let shared = shared_for("46007");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("set", &["c", "v", "ex", "100"], b"+OK\r\n");
    let ms = pttl_ms(&shared, "c");
    assert!(ms > 0 && ms <= 100_000, "lowercase ex pttl {ms}");
    e("set", &["c", "w", "kEePtTl"], b"+OK\r\n");
    let ms = pttl_ms(&shared, "c");
    assert!(ms > 0 && ms <= 100_000, "mixed-case keepttl pttl {ms}");
    e("set", &["n2", "x", "Nx"], b"+OK\r\n");
    e("set", &["n2", "y", "nX"], b"$-1\r\n");
    e("set", &["n3", "y", "xX"], b"$-1\r\n");
    e("set", &["c", "z", "gEt"], b"$1\r\nw\r\n");
}

#[test]
fn mset_mget_order_missing_and_crossslot() {
    let shared = shared_for("46008");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // Hash-tag co-located keys: one batch, +OK, reads back in order.
    e("mset", &["{t}a", "1", "{t}b", "2"], b"+OK\r\n");
    e("get", &["{t}a"], b"$1\r\n1\r\n");
    e("get", &["{t}b"], b"$1\r\n2\r\n");
    // MGET keeps the argument order; missing keys read as null bulks.
    e(
        "mget",
        &["{t}b", "{t}missing", "{t}a"],
        b"*3\r\n$1\r\n2\r\n$-1\r\n$1\r\n1\r\n",
    );
    e("mget", &["{t}missing"], b"*1\r\n$-1\r\n");
    // Arity: zero keys names the command; an odd tail is dedicated.
    e(
        "mset",
        &[],
        b"-ERR wrong number of arguments for 'mset' command\r\n",
    );
    e(
        "mset",
        &["{t}a", "v", "{t}b"],
        b"-ERR wrong number of arguments: 3\r\n",
    );
    e(
        "mget",
        &[],
        b"-ERR wrong number of arguments for 'mget' command\r\n",
    );
    // MGET crossing slots fails whole-command before any read.
    e(
        "mget",
        &["{t}a", "faraway"],
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n",
    );
    // MSET crossing slots is rejected before any mutation.
    e(
        "mset",
        &["{t}a", "v", "faraway", "w"],
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n",
    );
    e("get", &["faraway"], b"$-1\r\n");
    // MGET over a non-string key fails with WRONGTYPE (Redis).
    e("hset", &["{t}h", "f", "1"], b":1\r\n");
    e(
        "mget",
        &["{t}a", "{t}h"],
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    );
}
