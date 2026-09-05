//! Wire-level E2E for five "gap" commands: HMSET, ZREVRANGE, SINTERCARD,
//! LMPOP and XREVRANGE. Everything speaks raw RESP2 through a real
//! `resp::serve` listener (cluster-shaped commands: slot-derived physical
//! prefixes, hash-tag CROSSSLOT validation, MULTI queue routing), so the
//! exact reply BYTES are asserted end to end. The listener binds an
//! ephemeral port; per-test `conf.bind` ports 32801+ keep the RocksDB
//! store dirs apart (same harness shape as `resp_e2e.rs`, whose
//! `state::testutil` is lib-internal and invisible here).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rdb::{conf, hash, monitor, resp, state, store, topology};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN: &str = "test-token";
const WRONGTYPE: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
const CROSSSLOT: &[u8] = b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n";
const NUMKEYS: &[u8] = b"-ERR numkeys should be greater than 0\r\n";
const TOO_MANY_KEYS: &[u8] = b"-ERR Number of keys can't be greater than number of args\r\n";
const SYNTAX: &[u8] = b"-ERR syntax error\r\n";
const POSITIVE: &[u8] = b"-ERR value is out of range, must be positive\r\n";
const NOT_INT: &[u8] = b"-ERR value is not an integer or out of range\r\n";
const BAD_ID: &[u8] = b"-ERR Invalid stream ID specified as stream command argument\r\n";

/// One wire round-trip: send the space-joined `argv` (a TRAILING space
/// yields a final empty argument, e.g. `"hmset k f "` -> f=""), then
/// byte-compare the exact reply. Semicolon separators keep rustfmt from
/// exploding the one-line invocations.
macro_rules! r {
    ($s:expr; $argv:expr; $want:expr) => {
        rpc($s, &$argv.split(' ').collect::<Vec<&str>>(), $want).await
    };
}

/// `-ERR wrong number of arguments for '<cmd>' command\r\n`.
fn arity(cmd: &str) -> Vec<u8> {
    format!("-ERR wrong number of arguments for '{cmd}' command\r\n").into_bytes()
}

fn shared_for(port: u16) -> Arc<state::Shared> {
    let c = conf::Config {
        bind: format!("127.0.0.1:{port}"),
        store_path: "/tmp/".to_string(),
        raft_tcp_address: format!("127.0.0.1:{}", port + 100),
        raft_token: TOKEN.to_string(),
        ..Default::default()
    };
    let dir = std::env::temp_dir().join(format!("rdb-gaps-e2e-{}-{port}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = store::data_path(dir.to_str().unwrap(), &c.bind);
    let st = store::open(path.to_str().unwrap()).unwrap();
    Arc::new(state::Shared {
        mode: state::Mode::Normal,
        store: Arc::new(st),
        topology: Arc::new(RwLock::new(topology::empty())),
        raft: Arc::new(RwLock::new(state::stub_raft(&c))),
        monitor: Arc::new(monitor::new_collector()),
        latch: rdb::ds::latch::Latch::new(),
        wait_hub: rdb::ds::wait::WaitHub::new(),
        lite: Arc::new(rdb::lite::new_runtime()),
        sql_ts: Arc::new(rdb::sql::tx::Oracle::new()),
        migrating: Arc::new(RwLock::new(std::collections::HashMap::new())),
        importing: Arc::new(RwLock::new(std::collections::HashMap::new())),
        migrate_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        conf: c,
    })
}

/// Serve on an ephemeral port; return one AUTHed connection.
async fn authed_conn(port: u16) -> TcpStream {
    let listener = resp::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(resp::serve(listener, shared_for(port)));
    let mut s = tokio::time::timeout(TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect");
    rpc(&mut s, &["AUTH", TOKEN], b"+OK\r\n").await;
    s
}

/// RESP request frame for one command line (space-joined argv).
fn req(line: &str) -> Vec<u8> {
    let parts: Vec<&str> = line.split(' ').collect();
    let mut v = format!("*{}\r\n", parts.len()).into_bytes();
    for p in parts {
        v.extend_from_slice(format!("${}\r\n{p}\r\n", p.len()).as_bytes());
    }
    v
}

/// Send one command, read exactly `expect.len()` bytes, byte-compare.
async fn rpc(s: &mut TcpStream, parts: &[&str], expect: &[u8]) {
    s.write_all(&req(&parts.join(" "))).await.expect("write");
    let mut buf = vec![0u8; expect.len()];
    tokio::time::timeout(TIMEOUT, s.read_exact(&mut buf))
        .await
        .unwrap_or_else(|_| panic!("read timeout for {parts:?}, expected {expect:?}"))
        .expect("read");
    assert_eq!(buf, expect, "reply mismatch for {parts:?}");
}

/// A flat array reply from space-joined bulks: `arr("a b c")`.
fn arr(items: &str) -> Vec<u8> {
    let mut v = format!("*{}\r\n", items.split(' ').count()).into_bytes();
    for i in items.split(' ') {
        v.extend_from_slice(format!("${}\r\n{i}\r\n", i.len()).as_bytes());
    }
    v
}

/// A bulk string reply.
fn bulk(v: &str) -> Vec<u8> {
    format!("${}\r\n{v}\r\n", v.len()).into_bytes()
}

/// An LMPOP success reply: `[key, [space-joined elements...]]`.
fn pop_reply(key: &str, elems: &str) -> Vec<u8> {
    let mut v = format!("*2\r\n${}\r\n{key}\r\n", key.len()).into_bytes();
    v.extend_from_slice(&arr(elems));
    v
}

/// One stream entry frame `[id, [f, v]]` (single f=v field, as XADDed).
fn entry(id: &str) -> Vec<u8> {
    let mut v = format!("*2\r\n${}\r\n{id}\r\n", id.len()).into_bytes();
    v.extend_from_slice(b"*2\r\n$1\r\nf\r\n$1\r\nv\r\n");
    v
}

/// A stream range reply: `*n` header + entry frames, space-joined ids.
fn stream_reply(ids: &str) -> Vec<u8> {
    let mut v = format!("*{}\r\n", ids.split(' ').count()).into_bytes();
    for id in ids.split(' ') {
        v.extend_from_slice(&entry(id));
    }
    v
}

/// Two untagged keys hashing to DIFFERENT slots (for CROSSSLOT checks).
fn diff_slot_pair(seed: &str) -> (String, String) {
    let slot = |k: &str| hash::slot_number(hash::hash_tag(k.as_bytes()));
    let a = format!("{seed}-a");
    for i in 0..100 {
        let b = format!("{seed}-b{i}");
        if slot(&a) != slot(&b) {
            return (a, b);
        }
    }
    panic!("no differing-slot pair found for {seed}");
}

#[tokio::test]
async fn hmset_write_readback_overwrite_wrongtype_arity() {
    let mut s = authed_conn(32801).await;
    // Multi-field write replies the deprecated twin's plain +OK; HGETALL
    // walks fields in lexicographic order; overwriting an existing field
    // keeps the hash size; an empty value is a real value; a string key
    // is WRONGTYPE; odd argument counts are arity errors like HSET's.
    r!(&mut s; "hmset {h}k f1 v1 f2 v2"; b"+OK\r\n");
    r!(&mut s; "hget {h}k f1"; b"$2\r\nv1\r\n");
    r!(&mut s; "hget {h}k f2"; b"$2\r\nv2\r\n");
    r!(&mut s; "hget {h}k missing"; b"$-1\r\n");
    r!(&mut s; "hlen {h}k"; b":2\r\n");
    r!(&mut s; "hgetall {h}k"; &arr("f1 v1 f2 v2"));
    r!(&mut s; "hmset {h}k f1 longer"; b"+OK\r\n");
    r!(&mut s; "hlen {h}k"; b":2\r\n");
    r!(&mut s; "hget {h}k f1"; b"$6\r\nlonger\r\n");
    r!(&mut s; "hmset {h}e f "; b"+OK\r\n");
    r!(&mut s; "hget {h}e f"; b"$0\r\n\r\n");
    r!(&mut s; "set {h}s v"; b"+OK\r\n");
    r!(&mut s; "hmset {h}s f v"; WRONGTYPE);
    r!(&mut s; "hmset {h}k f"; &arity("hmset"));
    r!(&mut s; "hmset {h}k f v g"; &arity("hmset"));
}

#[tokio::test]
async fn zrevrange_windows_withscores_errors() {
    let mut s = authed_conn(32802).await;
    r!(&mut s; "zadd {z}z 1 a 2 b 3 c"; b":3\r\n");
    r!(&mut s; "hmset {z}h f v"; b"+OK\r\n");
    // [0,-1] is the full reversed rank order; WITHSCORES interleaves
    // member,score with SHORTEST-form scores ("3", not "3.0"); a negative
    // start counts from the back of the reversed order; [1,2] is a subset
    // window; a start clamped past the cardinality selects nothing; a
    // missing key is an empty array; a hash key is WRONGTYPE; an unknown
    // trailing option is a syntax error.
    r!(&mut s; "zrevrange {z}z 0 -1"; &arr("c b a"));
    r!(&mut s; "zrevrange {z}z 0 -1 WITHSCORES"; &arr("c 3 b 2 a 1"));
    r!(&mut s; "zrevrange {z}z -2 -1"; &arr("b a"));
    r!(&mut s; "zrevrange {z}z 1 2"; &arr("b a"));
    r!(&mut s; "zrevrange {z}z 5 10"; b"*0\r\n");
    r!(&mut s; "zrevrange {z}missing 0 -1"; b"*0\r\n");
    r!(&mut s; "zrevrange {z}h 0 -1"; WRONGTYPE);
    r!(&mut s; "zrevrange {z}z 0 -1 REV"; SYNTAX);
    r!(&mut s; "zrevrange {z}z"; &arity("zrevrange"));
}

#[tokio::test]
async fn zrevrange_fifty_members_head_is_reversed() {
    let mut s = authed_conn(32803).await;
    // 50 members m01..m50 scored 1..=50, pipelined in ONE write.
    let mut pipe = Vec::new();
    for i in 1..=50 {
        pipe.extend_from_slice(&req(&format!("zadd {{big}}z {i} m{i:02}")));
    }
    s.write_all(&pipe).await.expect("write zadds");
    let mut buf = vec![0u8; 4 * 50];
    tokio::time::timeout(TIMEOUT, s.read_exact(&mut buf))
        .await
        .expect("zadd replies timeout")
        .expect("zadd replies read");
    assert_eq!(buf, b":1\r\n".repeat(50), "every zadd added one member");
    // First five of the reversed order: m50 down to m46; the tail window
    // of the reversed order holds the lowest-scored members.
    r!(&mut s; "zrevrange {big}z 0 4"; &arr("m50 m49 m48 m47 m46"));
    r!(&mut s; "zrevrange {big}z 48 49"; &arr("m02 m01"));
}

#[tokio::test]
async fn sintercard_intersection_limit_and_numkeys_errors() {
    let mut s = authed_conn(32804).await;
    r!(&mut s; "sadd {t}a x y z"; b":3\r\n");
    r!(&mut s; "sadd {t}b y z w"; b":3\r\n");
    // Missing keys read as empty sets (intersection 0). LIMIT below the
    // cardinality answers the LIMIT (early stop); LIMIT 0 is unlimited;
    // LIMIT above the cardinality is the cardinality.
    r!(&mut s; "sintercard 2 {t}a {t}b"; b":2\r\n");
    r!(&mut s; "sintercard 2 {t}no1 {t}no2"; b":0\r\n");
    r!(&mut s; "sintercard 2 {t}a {t}no"; b":0\r\n");
    r!(&mut s; "sintercard 2 {t}a {t}b LIMIT 1"; b":1\r\n");
    r!(&mut s; "sintercard 2 {t}a {t}b LIMIT 0"; b":2\r\n");
    r!(&mut s; "sintercard 2 {t}a {t}b LIMIT 9"; b":2\r\n");
    // numkeys: 0 / negative / non-numeric; numkeys exceeding the provided
    // key arguments; LIMIT with a non-numeric value, a dangling token or
    // a junk token.
    r!(&mut s; "sintercard 0 {t}a {t}b"; NUMKEYS);
    r!(&mut s; "sintercard -1 {t}a {t}b"; NUMKEYS);
    r!(&mut s; "sintercard x {t}a {t}b"; NUMKEYS);
    r!(&mut s; "sintercard 3 {t}a {t}b"; TOO_MANY_KEYS);
    r!(&mut s; "sintercard 2 {t}a {t}b LIMIT x"; NOT_INT);
    r!(&mut s; "sintercard 2 {t}a {t}b LIMIT"; SYNTAX);
    r!(&mut s; "sintercard 2 {t}a {t}b junk"; SYNTAX);
    // Single-key form: the set's own cardinality; arity floor; untagged
    // keys in different slots.
    r!(&mut s; "sintercard 1 {t}k"; b":0\r\n");
    r!(&mut s; "sadd {t}k p q r"; b":3\r\n");
    r!(&mut s; "sintercard 1 {t}k"; b":3\r\n");
    r!(&mut s; "sintercard 1"; &arity("sintercard"));
    let (x, y) = diff_slot_pair("sc");
    r!(&mut s; &format!("sintercard 2 {x} {y}"); CROSSSLOT);
}

#[tokio::test]
async fn lmpop_ends_count_clamp_multikey_drain_and_errors() {
    let mut s = authed_conn(32805).await;
    r!(&mut s; "rpush {g}a 1 2 3"; b":3\r\n");
    // LEFT pops the head, RIGHT the tail, and the reply names the key;
    // COUNT above the list length is clamped to what remains; the drained
    // list is deleted (empty lists do not exist); COUNT must be positive
    // (0 is rejected before any key is read).
    r!(&mut s; "lmpop 1 {g}a LEFT"; &pop_reply("{g}a", "1"));
    r!(&mut s; "lmpop 1 {g}a RIGHT"; &pop_reply("{g}a", "3"));
    r!(&mut s; "lmpop 1 {g}a LEFT COUNT 10"; &pop_reply("{g}a", "2"));
    r!(&mut s; "exists {g}a"; b":0\r\n");
    r!(&mut s; "lmpop 1 {g}a LEFT COUNT 0"; POSITIVE);
    // First non-empty candidate wins: {g}first is missing, {g}second has
    // data -- the pop comes from {g}second and the reply names it. Once
    // every candidate is missing/empty: the null array.
    r!(&mut s; "rpush {g}second 7 8"; b":2\r\n");
    r!(&mut s; "lmpop 2 {g}first {g}second LEFT"; &pop_reply("{g}second", "7"));
    r!(&mut s; "lmpop 2 {g}second {g}no RIGHT COUNT 5"; &pop_reply("{g}second", "8"));
    r!(&mut s; "exists {g}second"; b":0\r\n");
    r!(&mut s; "lmpop 2 {g}no1 {g}no2 RIGHT"; b"*-1\r\n");
    // WRONGTYPE against a hash candidate.
    r!(&mut s; "hmset {g}h f v"; b"+OK\r\n");
    r!(&mut s; "lmpop 1 {g}h LEFT"; WRONGTYPE);
    // numkeys: 0 / negative / non-numeric; numkeys exceeding the key
    // arguments. A second key keeps the arity floor satisfied so the
    // missing/unknown-direction checks are what fire. Dangling COUNT and
    // the dispatch arity floor (argv[numkeys key] must exist).
    r!(&mut s; "lmpop 0 {g}a LEFT"; NUMKEYS);
    r!(&mut s; "lmpop -1 {g}a LEFT"; NUMKEYS);
    r!(&mut s; "lmpop x {g}a LEFT"; NUMKEYS);
    r!(&mut s; "lmpop 4 {g}a {g}b LEFT"; TOO_MANY_KEYS);
    r!(&mut s; "lmpop 1 {g}a {g}b"; SYNTAX);
    r!(&mut s; "lmpop 1 {g}a up"; SYNTAX);
    r!(&mut s; "lmpop 1 {g}a LEFT COUNT"; SYNTAX);
    r!(&mut s; "lmpop 1"; &arity("lmpop"));
    // Untagged keys in different slots.
    let (x, y) = diff_slot_pair("lp");
    r!(&mut s; &format!("lmpop 2 {x} {y} LEFT"); CROSSSLOT);
    // Interleaved with LPOP/RPOP: the state stays consistent.
    r!(&mut s; "rpush {g}i a b c d"; b":4\r\n");
    r!(&mut s; "lmpop 1 {g}i LEFT COUNT 2"; &pop_reply("{g}i", "a b"));
    r!(&mut s; "lpop {g}i"; b"$1\r\nc\r\n");
    r!(&mut s; "rpop {g}i"; b"$1\r\nd\r\n");
    r!(&mut s; "llen {g}i"; b":0\r\n");
}

#[tokio::test]
async fn xrevrange_ordering_bounds_count_and_errors() {
    let mut s = authed_conn(32806).await;
    for id in ["5-1", "5-2", "6-1"] {
        r!(&mut s; &format!("xadd s/q {id} f v"); &bulk(id));
    }
    // "+ -" is the full range newest-first; explicit ids take the END
    // first (same window, still descending); exclusive "(" bounds at BOTH
    // ends keep only the strictly-inside entry; COUNT caps at the newest
    // n; XRANGE over the same window returns the entries in the opposite
    // order; a missing stream is an empty array; bad ids on either bound
    // are rejected; a bare parent name is not a stream.
    r!(&mut s; "xrevrange s/q + -"; &stream_reply("6-1 5-2 5-1"));
    r!(&mut s; "xrevrange s/q 6-1 5-1"; &stream_reply("6-1 5-2 5-1"));
    r!(&mut s; "xrevrange s/q (6-1 (5-1"; &stream_reply("5-2"));
    r!(&mut s; "xrevrange s/q + - COUNT 2"; &stream_reply("6-1 5-2"));
    r!(&mut s; "xrange s/q - +"; &stream_reply("5-1 5-2 6-1"));
    r!(&mut s; "xrevrange s/none + -"; b"*0\r\n");
    r!(&mut s; "xrevrange s/q nonsense -"; BAD_ID);
    r!(&mut s; "xrevrange s/q + 1-x"; BAD_ID);
    r!(&mut s; "xrevrange s/q +"; &arity("xrevrange"));
    r!(&mut s; "xrevrange s/q + - BOGUS 2"; &arity("xrevrange"));
    r!(&mut s; "xrevrange s/q + - COUNT 1 x"; &arity("xrevrange"));
    r!(&mut s; "xrevrange s + -"; b"-ERR a full stream name 'parent/child' is required\r\n");
    // Single-entry stream.
    r!(&mut s; "xadd s/one 1-1 f v"; b"$3\r\n1-1\r\n");
    r!(&mut s; "xrevrange s/one + -"; &stream_reply("1-1"));
}

#[tokio::test]
async fn multi_exec_queues_hmset_zrevrange_lmpop_and_xrevrange() {
    let mut s = authed_conn(32807).await;
    r!(&mut s; "zadd {g}z 1 c"; b":1\r\n");
    // Same-slot keys queue cleanly: EXEC returns [OK, array, null array]
    // (LMPOP's candidate is missing inside the EXEC).
    r!(&mut s; "multi"; b"+OK\r\n");
    r!(&mut s; "hmset {g}h f v"; b"+QUEUED\r\n");
    r!(&mut s; "zrevrange {g}z 0 -1"; b"+QUEUED\r\n");
    r!(&mut s; "lmpop 1 {g}l LEFT"; b"+QUEUED\r\n");
    let mut want = b"*3\r\n+OK\r\n".to_vec();
    want.extend_from_slice(&arr("c"));
    want.extend_from_slice(b"*-1\r\n");
    r!(&mut s; "exec"; &want);
    // The queued write landed.
    r!(&mut s; "hget {g}h f"; b"$1\r\nv\r\n");
    // XREVRANGE is whitelisted (keyless queue shape): it queues without
    // slot routing and runs inside EXEC.
    r!(&mut s; "xadd s/q 1-1 f v"; b"$3\r\n1-1\r\n");
    r!(&mut s; "multi"; b"+OK\r\n");
    r!(&mut s; "xrevrange s/q + -"; b"+QUEUED\r\n");
    let mut want = b"*1\r\n".to_vec();
    want.extend_from_slice(&stream_reply("1-1"));
    r!(&mut s; "exec"; &want);
}

#[tokio::test]
async fn cross_command_views_lrange_and_sinterstore() {
    let mut s = authed_conn(32808).await;
    // LMPOP removals are exactly what LRANGE afterwards sees.
    r!(&mut s; "rpush {v}a 1 2 3 4"; b":4\r\n");
    r!(&mut s; "lmpop 1 {v}a LEFT COUNT 2"; &pop_reply("{v}a", "1 2"));
    r!(&mut s; "lrange {v}a 0 -1"; &arr("3 4"));
    r!(&mut s; "llen {v}a"; b":2\r\n");
    // SINTERCARD agrees with SINTERSTORE + SCARD on the same operands.
    r!(&mut s; "sadd {v}s1 a b c d"; b":4\r\n");
    r!(&mut s; "sadd {v}s2 a b c"; b":3\r\n");
    r!(&mut s; "sintercard 2 {v}s1 {v}s2"; b":3\r\n");
    r!(&mut s; "sinterstore {v}dst {v}s1 {v}s2"; b":3\r\n");
    r!(&mut s; "scard {v}dst"; b":3\r\n");
}
