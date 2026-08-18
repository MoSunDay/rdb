//! End-to-end tests for the JSON family: in-process lifecycle through
//! the real command registry with the REAL slot-prefix derivation (no
//! spawned node needed -- every command is route-local). `expect`
//! asserts exact RESP reply bytes.

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
    let dir = std::env::temp_dir().join(format!("rdb-json-e2e-{}-{tag}", std::process::id()));
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
/// first key arg, one current-thread runtime per call.
fn call(shared: &state::Shared, name: &str, args: &[&str]) -> Vec<u8> {
    let handler = command::lookup(name).unwrap_or_else(|| panic!("'{name}' not registered"));
    let prefix_key = args
        .first()
        .map(|a| hash::slot_with_prefix(hash::hash_tag(a.as_bytes())).1)
        .unwrap_or_default();
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

#[test]
fn set_get_roundtrip_and_type() {
    let shared = shared_for("45301");
    expect(
        &shared,
        "json.set",
        &["doc", ".", "{\"b\":1,\"a\":[true,null]}"],
        b"+OK\r\n",
    );
    // Byte-exact roundtrip: insertion order kept, no reformatting.
    expect(
        &shared,
        "json.get",
        &["doc"],
        b"$23\r\n{\"b\":1,\"a\":[true,null]}\r\n",
    );
    expect(&shared, "json.get", &["doc", ".a[0]"], b"$4\r\ntrue\r\n");
    expect(&shared, "json.type", &["doc"], b"+object\r\n");
    expect(&shared, "json.type", &["doc", ".b"], b"+integer\r\n");
    expect(&shared, "json.type", &["doc", ".a[1]"], b"+null\r\n");
    expect(&shared, "json.get", &["missing"], b"$-1\r\n");
}

#[test]
fn set_conditions_and_nested_mutation() {
    let shared = shared_for("45302");
    expect(&shared, "json.set", &["k", ".x.y", "1", "XX"], b"$-1\r\n");
    expect(&shared, "json.set", &["k", ".", "{}"], b"+OK\r\n");
    expect(&shared, "json.set", &["k", ".x.y", "[1,2]"], b"+OK\r\n");
    expect(&shared, "json.set", &["k", ".", "9", "NX"], b"$-1\r\n");
    // Nested mutation replaces only the addressed element.
    expect(
        &shared,
        "json.set",
        &["k", ".x.y[1]", "{\"z\":3}"],
        b"+OK\r\n",
    );
    expect(
        &shared,
        "json.get",
        &["k"],
        b"$23\r\n{\"x\":{\"y\":[1,{\"z\":3}]}}\r\n",
    );
    expect(
        &shared,
        "json.set",
        &["k", ".x.y[9]", "1"],
        b"-ERR path .x.y[9] does not exist\r\n",
    );
}

#[test]
fn string_append_and_number_increment() {
    let shared = shared_for("45303");
    expect(
        &shared,
        "json.set",
        &["k", ".", "{\"s\":\"ab\",\"n\":1}"],
        b"+OK\r\n",
    );
    expect(&shared, "json.strappend", &["k", ".s", "\"cd\""], b":4\r\n");
    expect(&shared, "json.get", &["k", ".s"], b"$6\r\n\"abcd\"\r\n");
    expect(&shared, "json.strlen", &["k", ".s"], b":4\r\n");
    expect(&shared, "json.numincrby", &["k", ".n", "2"], b"$1\r\n3\r\n");
    expect(
        &shared,
        "json.numincrby",
        &["k", ".n", "0.5"],
        b"$3\r\n3.5\r\n",
    );
    expect(
        &shared,
        "json.numincrby",
        &["k", ".s", "1"],
        b"-ERR wrong type of path value\r\n",
    );
    expect(
        &shared,
        "json.strappend",
        &["k", ".s", "1"],
        b"-ERR wrong value type: expected string\r\n",
    );
}

#[test]
fn array_lifecycle() {
    let shared = shared_for("45304");
    expect(&shared, "json.set", &["k", ".", "[]"], b"+OK\r\n");
    expect(
        &shared,
        "json.arrappend",
        &["k", ".", "1", "2", "3"],
        b":3\r\n",
    );
    expect(&shared, "json.arrlen", &["k"], b":3\r\n");
    expect(&shared, "json.arrindex", &["k", ".", "2"], b":1\r\n");
    expect(&shared, "json.arrindex", &["k", ".", "2", "2"], b":-1\r\n");
    expect(&shared, "json.arrinsert", &["k", ".", "0", "0"], b":4\r\n");
    expect(&shared, "json.arrpop", &["k"], b"$1\r\n3\r\n");
    expect(&shared, "json.arrpop", &["k", ".", "-3"], b"$1\r\n0\r\n");
    expect(
        &shared,
        "json.arrpop",
        &["k", ".", "9"],
        b"-ERR index out of range\r\n",
    );
    expect(&shared, "json.arrtrim", &["k", ".", "0", "0"], b":1\r\n");
    expect(&shared, "json.get", &["k"], b"$3\r\n[1]\r\n");
}

#[test]
fn object_reads_and_path_delete() {
    let shared = shared_for("45305");
    expect(
        &shared,
        "json.set",
        &["k", ".", "{\"o\":{\"b\":2,\"a\":1},\"keep\":9}"],
        b"+OK\r\n",
    );
    expect(
        &shared,
        "json.objkeys",
        &["k", ".o"],
        b"*2\r\n$1\r\nb\r\n$1\r\na\r\n",
    );
    expect(&shared, "json.objlen", &["k", ".o"], b":2\r\n");
    expect(
        &shared,
        "json.objlen",
        &["k", ".o.b"],
        b"-ERR wrong type of path value\r\n",
    );
    expect(&shared, "json.del", &["k", ".o.b"], b":1\r\n");
    expect(
        &shared,
        "json.get",
        &["k"],
        b"$22\r\n{\"o\":{\"a\":1},\"keep\":9}\r\n",
    );
    expect(&shared, "json.del", &["k", ".o.b"], b":0\r\n");
    expect(&shared, "json.forget", &["k"], b":1\r\n");
    expect(&shared, "json.get", &["k"], b"$-1\r\n");
    expect(&shared, "del", &["k"], b":0\r\n");
}

#[test]
fn mget_with_hash_tags() {
    let shared = shared_for("45306");
    expect(
        &shared,
        "json.set",
        &["{tag}a", ".", "{\"v\":1}"],
        b"+OK\r\n",
    );
    expect(
        &shared,
        "json.set",
        &["{tag}b", ".", "{\"v\":2}"],
        b"+OK\r\n",
    );
    expect(
        &shared,
        "json.mget",
        &["{tag}a", "{tag}b", "{tag}c", ".v"],
        b"*3\r\n$1\r\n1\r\n$1\r\n2\r\n$-1\r\n",
    );
    expect(
        &shared,
        "json.mget",
        &["{tag}a", "other", ".v"],
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n",
    );
}

#[test]
fn multipath_get_is_a_flat_array() {
    let shared = shared_for("45307");
    expect(
        &shared,
        "json.set",
        &["k", ".", "{\"a\":1,\"b\":2}"],
        b"+OK\r\n",
    );
    expect(
        &shared,
        "json.get",
        &["k", ".a", ".zzz", ".b"],
        b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n",
    );
}

#[test]
fn wrong_type_keys_are_rejected() {
    let shared = shared_for("45308");
    expect(&shared, "set", &["raw", "x"], b"+OK\r\n");
    expect(
        &shared,
        "json.get",
        &["raw"],
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    );
    expect(
        &shared,
        "json.set",
        &["raw", ".", "1"],
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    );
    expect(
        &shared,
        "json.type",
        &["raw"],
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n",
    );
}

#[test]
fn ttl_survives_mutations_and_expires() {
    let shared = shared_for("45309");
    expect(&shared, "json.set", &["k", ".", "{\"n\":1}"], b"+OK\r\n");
    // PEXPIREAT migrates the record into the enveloped TTL shape; json
    // writes after it must KEEP the deadline (not reset it to 0).
    expect(&shared, "pexpireat", &["k", "9999999999999"], b":1\r\n");
    expect(&shared, "json.numincrby", &["k", ".n", "1"], b"$1\r\n2\r\n");
    // Deadline kept (remaining TTL still ~ the full horizon, not -1/-2).
    let ttl = call(&shared, "ttl", &["k"]);
    let secs: i64 = std::str::from_utf8(&ttl[1..ttl.len() - 2])
        .unwrap()
        .parse()
        .unwrap();
    assert!(secs > 1_000_000_000, "ttl lost after json write: {secs}");
    // A past deadline drops the record on the write itself.
    expect(&shared, "pexpireat", &["k", "1"], b":1\r\n");
    expect(&shared, "json.get", &["k"], b"$-1\r\n");
    expect(&shared, "exists", &["k"], b":0\r\n");
}
