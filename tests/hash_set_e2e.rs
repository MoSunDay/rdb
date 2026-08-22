//! End-to-end tests for the hash and set commands: dispatch through the
//! real command registry with slot-derived prefixes (mirroring the RESP
//! layer), RocksDB-backed `Shared`, TTL via the EXPIRE family, and the
//! CROSSSLOT rule for multi-key set algebra.

use std::sync::{Arc, RwLock};

use rdb::{command, conf, hash, monitor, state, store, topology};

/// Mirror of `state::testutil::shared_with` (lib-internal, invisible
/// here); each test gets its own bind, store dir and tag.
fn shared_for(tag: &str) -> state::Shared {
    let mut conf = conf::Config {
        bind: "127.0.0.1:43681".to_string(),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: "127.0.0.1:23681".to_string(),
        raft_token: "test-token".to_string(),
        ..Default::default()
    };
    conf.bind = format!("127.0.0.1:{tag}");
    let dir = std::env::temp_dir().join(format!("rdb-hs-e2e-{}-{tag}", std::process::id()));
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
        sql_ts: std::sync::Arc::new(rdb::sql::tx::Oracle::new()),
        conf,
    }
}

/// Dispatch like the RESP layer: registry lookup, slot prefix from the
/// first key arg (empty for whitelisted/keyless commands), one
/// current-thread runtime per call.
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

#[test]
fn hash_lifecycle_through_registry() {
    let shared = shared_for("43001");
    // Two new fields in one HSET; updates then count only the new one.
    assert_eq!(
        call(&shared, "hset", &[b"h", b"f1", b"v1", b"f2", b"v2"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hset", &[b"h", b"f1", b"x", b"f3", b"v3"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(call(&shared, "type", &[b"h"]), b"+hash\r\n".to_vec());
    assert_eq!(
        call(&shared, "hget", &[b"h", b"f1"]),
        b"$1\r\nx\r\n".to_vec()
    );
    // HGETALL walks fields in lexicographic order.
    assert_eq!(
        call(&shared, "hgetall", &[b"h"]),
        b"*6\r\n$2\r\nf1\r\n$1\r\nx\r\n$2\r\nf2\r\n$2\r\nv2\r\n$2\r\nf3\r\n$2\r\nv3\r\n".to_vec()
    );
    // Counters live behind the same meta.
    assert_eq!(
        call(&shared, "hincrby", &[b"h", b"n", b"41"]),
        b":41\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hincrbyfloat", &[b"h", b"pi", b"0.25"]),
        b"$4\r\n0.25\r\n".to_vec()
    );
    assert_eq!(call(&shared, "hlen", &[b"h"]), b":5\r\n".to_vec());
    // Deleting every field removes the key entirely.
    assert_eq!(
        call(&shared, "hdel", &[b"h", b"f1", b"f2", b"f3", b"n", b"pi"]),
        b":5\r\n".to_vec()
    );
    assert_eq!(call(&shared, "exists", &[b"h"]), b":0\r\n".to_vec());
    assert_eq!(call(&shared, "type", &[b"h"]), b"+none\r\n".to_vec());
    assert_eq!(call(&shared, "hgetall", &[b"h"]), b"*0\r\n".to_vec());
}

#[test]
fn hash_ttl_lazy_expiry_via_expire_family() {
    let shared = shared_for("43002");
    call(&shared, "hset", &[b"{t}h", b"a", b"1", b"b", b"2"]);
    // EXPIRE rewrites the hash META envelope (kind + count preserved).
    assert_eq!(
        call(&shared, "expire", &[b"{t}h", b"60"]),
        b":1\r\n".to_vec()
    );
    let ttl = String::from_utf8(call(&shared, "ttl", &[b"{t}h"])).unwrap();
    assert!(ttl == ":60\r\n" || ttl == ":59\r\n", "ttl {ttl}");
    assert_eq!(call(&shared, "hlen", &[b"{t}h"]), b":2\r\n".to_vec());
    // A short real deadline: reads after it lazily purge meta + fields.
    assert_eq!(
        call(&shared, "pexpire", &[b"{t}h", b"110"]),
        b":1\r\n".to_vec()
    );
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert_eq!(call(&shared, "hgetall", &[b"{t}h"]), b"*0\r\n".to_vec());
    assert_eq!(call(&shared, "exists", &[b"{t}h"]), b":0\r\n".to_vec());
    // The purged family is really gone, index entry included.
    assert_eq!(call(&shared, "del", &[b"{t}h"]), b":0\r\n".to_vec());
}

#[test]
fn hash_scan_full_page_cursor_reset() {
    let shared = shared_for("43003");
    call(&shared, "hset", &[b"hh", b"f1", b"1", b"f2", b"2"]);
    // Fewer fields than the COUNT hint: one page, cursor back to "0".
    assert_eq!(
        call(&shared, "hscan", &[b"hh", b"0"]),
        b"*2\r\n$1\r\n0\r\n*2\r\n$2\r\nf1\r\n$2\r\nf2\r\n".to_vec()
    );
    // WITHVALUES flattens the pairs.
    assert_eq!(
        call(&shared, "hscan", &[b"hh", b"0", b"WITHVALUES"]),
        b"*2\r\n$1\r\n0\r\n*4\r\n$2\r\nf1\r\n$1\r\n1\r\n$2\r\nf2\r\n$1\r\n2\r\n".to_vec()
    );
}

#[test]
fn set_lifecycle_through_registry() {
    let shared = shared_for("43004");
    assert_eq!(
        call(&shared, "sadd", &[b"s", b"b", b"a", b"c"]),
        b":3\r\n".to_vec()
    );
    assert_eq!(call(&shared, "type", &[b"s"]), b"+set\r\n".to_vec());
    assert_eq!(call(&shared, "scard", &[b"s"]), b":3\r\n".to_vec());
    // SMEMBERS returns members in lexicographic order.
    assert_eq!(
        call(&shared, "smembers", &[b"s"]),
        b"*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sismember", &[b"s", b"a"]),
        b":1\r\n".to_vec()
    );
    // Removing every member removes the key.
    assert_eq!(
        call(&shared, "srem", &[b"s", b"a", b"b", b"c"]),
        b":3\r\n".to_vec()
    );
    assert_eq!(call(&shared, "exists", &[b"s"]), b":0\r\n".to_vec());
    assert_eq!(call(&shared, "type", &[b"s"]), b"+none\r\n".to_vec());
    // A string key cannot masquerade as a set.
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(
        call(&shared, "sadd", &[b"str", b"x"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn set_algebra_smove_and_crossslot() {
    let shared = shared_for("43005");
    call(&shared, "sadd", &[b"{g}a", b"x", b"y"]);
    call(&shared, "sadd", &[b"{g}b", b"y", b"z"]);
    // Read variants are sorted; missing keys read as empty sets.
    assert_eq!(
        call(&shared, "sunion", &[b"{g}a", b"{g}b", b"{g}none"]),
        b"*3\r\n$1\r\nx\r\n$1\r\ny\r\n$1\r\nz\r\n".to_vec()
    );
    // SUNIONSTORE overwrites an existing destination of the same kind.
    call(&shared, "sadd", &[b"{g}dst", b"stale"]);
    assert_eq!(
        call(&shared, "sunionstore", &[b"{g}dst", b"{g}a", b"{g}b"]),
        b":3\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "smembers", &[b"{g}dst"]),
        b"*3\r\n$1\r\nx\r\n$1\r\ny\r\n$1\r\nz\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sinterstore", &[b"{g}dst", b"{g}a", b"{g}b"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "smembers", &[b"{g}dst"]),
        b"*1\r\n$1\r\ny\r\n".to_vec()
    );
    // SMOVE migrates a member between sets in one batch.
    assert_eq!(
        call(&shared, "smove", &[b"{g}a", b"{g}b", b"x"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "smembers", &[b"{g}a"]),
        b"*1\r\n$1\r\ny\r\n".to_vec()
    );
    // A {u}-tagged key lands in another slot: rejected before mutation.
    assert_eq!(
        call(&shared, "smove", &[b"{g}a", b"{u}far", b"y"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sdiffstore", &[b"{u}far", b"{g}a"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    assert_eq!(call(&shared, "scard", &[b"{g}a"]), b":1\r\n".to_vec());
}
