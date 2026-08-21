//! Integration tests for the EXPIRE family through the command layer:
//! handlers resolved from the real registry, driven via `command::Ctx`
//! against a real RocksDB store. Covers the raw-string -> enveloped
//! migration, TTL/PTTL/PERSIST roundtrip, lazy expiry on GET/MGET and
//! DEL/RENAME interplay with expiring keys.

use std::sync::{Arc, RwLock};

use rdb::{command, conf, hash, monitor, state, store, topology};

fn shared_for(tag: &str) -> state::Shared {
    let mut conf = conf::Config {
        bind: "127.0.0.1:32681".to_string(),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: "127.0.0.1:22681".to_string(),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    conf.bind = format!("127.0.0.1:{tag}");
    let dir = std::env::temp_dir().join(format!("rdb-exp-e2e-{}-{tag}", std::process::id()));
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
        lite: std::sync::Arc::new(rdb::lite::new_runtime()),
        sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
        conf,
    }
}

/// Dispatch like the RESP layer: registry lookup, slot prefix from the
/// first key arg (empty for whitelisted/keyless commands, mirroring
/// `resp::conn`), one current-thread runtime per call.
fn call(shared: &state::Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
    let handler = command::lookup(name).unwrap_or_else(|| panic!("'{name}' not registered"));
    let prefix_key = if rdb::router::is_whitelisted(name) {
        Vec::new()
    } else {
        args.first()
            .map(|a| hash::slot_with_prefix(hash::hash_tag(a)).1)
            .unwrap_or_default()
    };
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
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
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(handler(&mut ctx));
    out
}

fn set(shared: &state::Shared, key: &[u8], val: &[u8]) -> Vec<u8> {
    call(shared, "set", &[key, val])
}

#[test]
fn set_expire_ttl_persist_via_registry() {
    let shared = shared_for("42001");
    assert_eq!(set(&shared, b"k", b"v"), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "ttl", &[b"k"]), b":-1\r\n".to_vec());
    assert_eq!(call(&shared, "expire", &[b"k", b"60"]), b":1\r\n".to_vec());
    // GET still sees the value through the enveloped record.
    assert_eq!(call(&shared, "get", &[b"k"]), b"$1\r\nv\r\n".to_vec());
    assert_eq!(call(&shared, "type", &[b"k"]), b"+string\r\n".to_vec());
    // TTL floors the remaining milliseconds to seconds.
    let ttl = String::from_utf8(call(&shared, "ttl", &[b"k"])).unwrap();
    assert!(ttl == ":60\r\n" || ttl == ":59\r\n", "ttl {ttl}");
    // PERSIST migrates back to the bare raw-string shape.
    assert_eq!(call(&shared, "persist", &[b"k"]), b":1\r\n".to_vec());
    assert_eq!(call(&shared, "ttl", &[b"k"]), b":-1\r\n".to_vec());
    assert_eq!(call(&shared, "get", &[b"k"]), b"$1\r\nv\r\n".to_vec());
    let raw = rdb::ds::codec::string_key(&hash::slot_with_prefix(hash::hash_tag(b"k")).1, b"k");
    assert_eq!(
        rdb::store::ops::get_physical(&shared.store, &raw)
            .unwrap()
            .unwrap(),
        b"v".to_vec(),
        "PERSIST restores the bare record"
    );
}

#[test]
fn pexpireat_lazy_expiry_and_del_cleanup() {
    let shared = shared_for("42002");
    set(&shared, b"k", b"v");
    // Deadline far in the past: apply_ttl deletes the key outright.
    assert_eq!(
        call(&shared, "pexpireat", &[b"k", b"1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(call(&shared, "exists", &[b"k"]), b":0\r\n".to_vec());
    assert_eq!(call(&shared, "get", &[b"k"]), b"$-1\r\n".to_vec());

    // Short real TTL: after the deadline, reads lazily purge the record.
    set(&shared, b"mzz", b"v");
    assert_eq!(
        call(&shared, "pexpire", &[b"mzz", b"110"]),
        b":1\r\n".to_vec()
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(call(&shared, "get", &[b"mzz"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&shared, "ttl", &[b"mzz"]), b":-2\r\n".to_vec());
    // The purged record is really gone, index entry included: DEL says 0.
    assert_eq!(call(&shared, "del", &[b"mzz"]), b":0\r\n".to_vec());
}

#[test]
fn mget_reads_enveloped_and_missing_keys() {
    let shared = shared_for("42003");
    set(&shared, b"{e}a", b"1");
    set(&shared, b"{e}b", b"2");
    assert_eq!(
        call(&shared, "pexpire", &[b"{e}b", b"60000"]),
        b":1\r\n".to_vec()
    );
    // b now lives in an enveloped record; a stays raw; c is missing.
    let reply = call(&shared, "mget", &[b"{e}a", b"{e}b", b"{e}c"]);
    assert_eq!(reply, b"*3\r\n$1\r\n1\r\n$1\r\n2\r\n$-1\r\n".to_vec());
}

#[test]
fn rename_carries_ttl_and_overwrites_destination() {
    let shared = shared_for("42004");
    set(&shared, b"{e}src", b"v");
    assert_eq!(
        call(&shared, "pexpire", &[b"{e}src", b"60000"]),
        b":1\r\n".to_vec()
    );
    set(&shared, b"{e}dst", b"old");
    assert_eq!(
        call(&shared, "rename", &[b"{e}src", b"{e}dst"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(call(&shared, "get", &[b"{e}dst"]), b"$1\r\nv\r\n".to_vec());
    let pttl = String::from_utf8(call(&shared, "pttl", &[b"{e}dst"])).unwrap();
    let ms: i64 = pttl.trim_start_matches(':').trim_end().parse().unwrap();
    assert!(ms > 0 && ms <= 60_000, "pttl {ms}");
    assert_eq!(call(&shared, "exists", &[b"{e}src"]), b":0\r\n".to_vec());
    // RENAMENX refuses because dst now exists.
    set(&shared, b"{e}other", b"x");
    assert_eq!(
        call(&shared, "renamenx", &[b"{e}other", b"{e}dst"]),
        b":0\r\n".to_vec()
    );
}

#[test]
fn scan_and_keys_across_whole_instance_prefix() {
    let shared = shared_for("42005");
    // Whitelisted (keyless) commands get an empty prefix: the whole local
    // keyspace is scanned, crossing slots.
    set(&shared, b"aa", b"1");
    set(&shared, b"mzz", b"2");
    let keys_reply = String::from_utf8(call(&shared, "keys", &[b"*"])).unwrap();
    assert!(keys_reply.contains("aa"), "{keys_reply}");
    assert!(keys_reply.contains("m"), "{keys_reply}");

    let mut cursor = "0".to_string();
    let mut rounds = 0;
    loop {
        let reply = String::from_utf8(call(&shared, "scan", &[cursor.as_bytes()])).unwrap();
        assert!(reply.starts_with("*2"), "{reply}");
        let mut it = reply.split("\r\n");
        it.next();
        it.next();
        let next = it.next().unwrap().to_string();
        rounds += 1;
        if next == "0" {
            break;
        }
        cursor = next;
        assert!(rounds < 100, "scan did not terminate: {reply}");
    }
    assert!(
        call(&shared, "randomkey", &[]).len() > 6,
        "some key came back"
    );
}

/// RENAME must stay inside one slot: `{a}`-tagged source vs `{b}`-tagged
/// destination is CROSSSLOT (refused before any mutation), while a
/// same-slot `{t}`-tagged pair moves key + TTL together.
#[test]
fn rename_crossslot_rejected_and_same_slot_moves_ttl() {
    let shared = shared_for("42006");
    // Cross-slot: nothing moves, the source keeps key AND deadline.
    set(&shared, b"{a}src", b"v");
    assert_eq!(
        call(&shared, "pexpire", &[b"{a}src", b"60000"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "rename", &[b"{a}src", b"{b}dst"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    assert_eq!(call(&shared, "get", &[b"{a}src"]), b"$1\r\nv\r\n".to_vec());
    let ms = String::from_utf8(call(&shared, "pttl", &[b"{a}src"]))
        .unwrap()
        .trim_start_matches(':')
        .trim_end()
        .parse::<i64>()
        .unwrap();
    assert!(
        ms > 0 && ms <= 60_000,
        "source ttl survived the refusal: {ms}"
    );
    assert_eq!(call(&shared, "exists", &[b"{b}dst"]), b":0\r\n".to_vec());
    // RENAMENX runs through the same guard.
    assert_eq!(
        call(&shared, "renamenx", &[b"{a}src", b"{b}dst"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    // Same-slot hash-tagged pair: the deadline travels with the key.
    set(&shared, b"{t}a", b"moved");
    assert_eq!(
        call(&shared, "pexpire", &[b"{t}a", b"60000"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "rename", &[b"{t}a", b"{t}b"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "get", &[b"{t}b"]),
        b"$5\r\nmoved\r\n".to_vec()
    );
    let ms = String::from_utf8(call(&shared, "pttl", &[b"{t}b"]))
        .unwrap()
        .trim_start_matches(':')
        .trim_end()
        .parse::<i64>()
        .unwrap();
    assert!(ms > 0 && ms <= 60_000, "moved ttl: {ms}");
    assert_eq!(call(&shared, "exists", &[b"{t}a"]), b":0\r\n".to_vec());
}
