//! End-to-end tests for the list family: in-process lifecycle through the
//! real command registry (push/read order, LREM compaction, LTRIM,
//! LINSERT/LPOS, LMOVE/RPOPLPUSH rotation, TTL interplay, blocking pops)
//! plus an over-the-wire BLPOP wake-up against a real spawned node.
//! `expect` asserts the exact RESP reply bytes of a dispatched command.

mod common;

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use common::{cmd_one_shot, contains_bytes, spawn_node, wait_resp_ready, TOKEN};
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
    let dir = std::env::temp_dir().join(format!("rdb-list-e2e-{}-{tag}", std::process::id()));
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

fn text(reply: &[u8]) -> String {
    String::from_utf8_lossy(reply).into_owned()
}

/// A flat RESP array of bulk payloads: `*N` + N `$len\r\n<bytes>\r\n`.
fn arr(items: &[&str]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", items.len()).into_bytes();
    for i in items {
        buf.extend_from_slice(format!("${}\r\n{i}\r\n", i.len()).as_bytes());
    }
    buf
}

/// One RESP array command frame over the wire.
fn frame(args: &[&str]) -> Vec<u8> {
    let mut buf = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        buf.extend_from_slice(a.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf
}

const CROSSSLOT: &[u8] = b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n";

#[test]
fn push_order_lrange_windows() {
    let shared = shared_for("44001");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("rpush", &["l", "a", "b", "c"], b":3\r\n");
    e("type", &["l"], b"+list\r\n");
    e("rpush", &["l", "d", "e"], b":5\r\n");
    e("lpush", &["l", "z"], b":6\r\n");
    e(
        "lrange",
        &["l", "0", "-1"],
        &arr(&["z", "a", "b", "c", "d", "e"]),
    );
    e("lrange", &["l", "0", "1"], &arr(&["z", "a"]));
    e("lrange", &["l", "-2", "-1"], &arr(&["d", "e"]));
    e("lrange", &["l", "10", "20"], b"*0\r\n");
    e("lrange", &["missing", "0", "-1"], b"*0\r\n");
    e("llen", &["l"], b":6\r\n");
    e("lindex", &["l", "0"], b"$1\r\nz\r\n");
    e("lindex", &["l", "-1"], b"$1\r\ne\r\n");
    e("lindex", &["l", "99"], b"$-1\r\n");
    e("lset", &["l", "0", "y"], b"+OK\r\n");
    e("lrange", &["l", "0", "0"], &arr(&["y"]));
}

#[test]
fn lrem_compaction_keeps_pops_correct() {
    let shared = shared_for("44002");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("rpush", &["r", "a", "b", "a", "c", "a"], b":5\r\n");
    // Remove 2 headmost "a"s: the tail one survives, then pops follow.
    e("lrem", &["r", "2", "a"], b":2\r\n");
    e("lrange", &["r", "0", "-1"], &arr(&["b", "c", "a"]));
    e("lpop", &["r"], b"$1\r\nb\r\n");
    e("rpop", &["r"], b"$1\r\na\r\n");
    e("lrange", &["r", "0", "-1"], &arr(&["c"]));
    e("lrem", &["r", "0", "x"], b":0\r\n");
    // Count 0 removes every occurrence; the emptied key disappears.
    e("rpush", &["r2", "x", "y", "x"], b":3\r\n");
    e("lrem", &["r2", "0", "x"], b":2\r\n");
    e("exists", &["r2"], b":1\r\n");
    e("lpop", &["r2"], b"$1\r\ny\r\n");
    e("exists", &["r2"], b":0\r\n");
    e("lrem", &["gone", "0", "x"], b":0\r\n");
}

#[test]
fn ltrim_linsert_lpos() {
    let shared = shared_for("44003");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("rpush", &["t", "a", "b", "c", "d", "e"], b":5\r\n");
    e("ltrim", &["t", "1", "3"], b"+OK\r\n");
    e("lrange", &["t", "0", "-1"], &arr(&["b", "c", "d"]));
    // Insert before/after a pivot; unknown pivots report -1.
    e("linsert", &["t", "BEFORE", "c", "x"], b":4\r\n");
    e("linsert", &["t", "AFTER", "c", "y"], b":5\r\n");
    e(
        "lrange",
        &["t", "0", "-1"],
        &arr(&["b", "x", "c", "y", "d"]),
    );
    e("linsert", &["t", "BEFORE", "zz", "w"], b":-1\r\n");
    e("lpos", &["t", "c"], b":2\r\n");
    e("lpos", &["t", "zz"], b"$-1\r\n");
    // LTRIM away everything: the key is deleted.
    e("ltrim", &["t", "99", "100"], b"+OK\r\n");
    e("exists", &["t"], b":0\r\n");
    e("lrange", &["t", "0", "-1"], b"*0\r\n");
}

#[test]
fn lmove_rpoplpush_rotation_and_crossslot() {
    let shared = shared_for("44004");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("rpush", &["{mv}src", "1", "2", "3"], b":3\r\n");
    e(
        "lmove",
        &["{mv}src", "{mv}src", "RIGHT", "LEFT"],
        b"$1\r\n3\r\n",
    );
    e("lrange", &["{mv}src", "0", "-1"], &arr(&["3", "1", "2"]));
    e(
        "lmove",
        &["{mv}src", "{mv}dst", "LEFT", "RIGHT"],
        b"$1\r\n3\r\n",
    );
    e("lrange", &["{mv}dst", "0", "-1"], &arr(&["3"]));
    e("rpoplpush", &["{mv}src", "{mv}dst"], b"$1\r\n2\r\n");
    e("lrange", &["{mv}dst", "0", "-1"], &arr(&["2", "3"]));
    // Empty source yields a null bulk, not an error.
    e("rpoplpush", &["{mv}none", "{mv}dst"], b"$-1\r\n");
    // Keys in one request must share a slot.
    let far = call(&shared, "lmove", &["a", "b", "LEFT", "LEFT"]);
    assert_eq!(far, CROSSSLOT.to_vec(), "in 'lmove'");
}

#[test]
fn list_ttl_lazy_purge_on_typed_read() {
    let shared = shared_for("44005");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("rpush", &["{t}lk", "a", "b", "c"], b":3\r\n");
    e("expire", &["{t}lk", "60"], b":1\r\n");
    let ttl = text(&call(&shared, "ttl", &["{t}lk"]));
    assert!(ttl == ":60\r\n" || ttl == ":59\r\n", "ttl {ttl}");
    // Reads still work while live (and refresh nothing).
    e("llen", &["{t}lk"], b":3\r\n");
    // A short real deadline: the typed read after it lazily purges meta
    // and every element record together.
    e("pexpire", &["{t}lk", "100"], b":1\r\n");
    std::thread::sleep(Duration::from_millis(250));
    e("llen", &["{t}lk"], b":0\r\n");
    e("exists", &["{t}lk"], b":0\r\n");
    let keys = text(&call(&shared, "keys", &["*"]));
    assert!(!keys.contains("{t}lk"), "purged key still listed: {keys}");
    e("del", &["{t}lk"], b":0\r\n");
}

#[test]
fn blpop_timeout_and_immediate_hit() {
    let shared = shared_for("44006");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("lpush", &["k", "v"], b":1\r\n");
    // Immediate hit on the latched quick path: *2 (key, element).
    e("blpop", &["k", "1"], &arr(&["k", "v"]));
    // The drained key now times out with the null array (~100ms parked).
    let started = Instant::now();
    e("blpop", &["k", "0.1"], b"*-1\r\n");
    let parked = started.elapsed();
    assert!(
        parked >= Duration::from_millis(90),
        "returned early: {parked:?}"
    );
    assert!(
        parked < Duration::from_secs(5),
        "parked too long: {parked:?}"
    );
}

/// BRPOP twin of the BLPOP pair above: the immediate hit must come off
/// the TAIL (not the head), and the drained key then times out with the
/// same null array (`*-1`). The over-the-wire wakeup path is shared
/// with BLPOP (`block_pop_cmd`), which `blpop_wakes_on_lpush_over_wire`
/// already drives.
#[test]
fn brpop_immediate_tail_hit_and_timeout() {
    let shared = shared_for("44008");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("rpush", &["rk", "head", "mid", "tail"], b":3\r\n");
    // Immediate hit on the latched quick path: *2 (key, TAIL element).
    e("brpop", &["rk", "1"], &arr(&["rk", "tail"]));
    // The head is still there -- only the right end was popped.
    e("lrange", &["rk", "0", "-1"], &arr(&["head", "mid"]));
    // The second pop takes the new tail; then the key is drained.
    e("brpop", &["rk", "0.1"], &arr(&["rk", "mid"]));
    e("brpop", &["rk", "0.1"], &arr(&["rk", "head"]));
    // Drained: the next BRPOP parks for its ~100ms and answers *-1.
    let started = Instant::now();
    e("brpop", &["rk", "0.1"], b"*-1\r\n");
    let parked = started.elapsed();
    assert!(
        parked >= Duration::from_millis(90),
        "returned early: {parked:?}"
    );
    assert!(
        parked < Duration::from_secs(5),
        "parked too long: {parked:?}"
    );
    // Multi-key BRPOP serves the FIRST non-empty key in argument order
    // (and multi-key crossing slots is CROSSSLOT, like BLPOP).
    e("rpush", &["{b}one", "x"], b":1\r\n");
    e("rpush", &["{b}two", "y"], b":1\r\n");
    e(
        "brpop",
        &["{b}one", "{b}two", "0.1"],
        &arr(&["{b}one", "x"]),
    );
    e(
        "brpop",
        &["{b}one", "{b}two", "0.1"],
        &arr(&["{b}two", "y"]),
    );
    e(
        "brpop",
        &["{b}one", "far", "0.1"],
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n",
    );
}

/// Regression for the lost-wakeup fix in `command::list_block`: the
/// latched quick-pop Got path used to return without unregistering its
/// shared waiter, so the next notify for that slot was swallowed by the
/// stale entry and a BLPOP parked afterwards slept into its deadline.
#[test]
fn blpop_got_does_not_swallow_next_notify() {
    let shared = Arc::new(shared_for("44007"));
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    // Seed data so this BLPOP returns via the quick-pop Got path (the
    // buggy return leaked a registered waiter for the slot).
    e("lpush", &["wk", "first"], b":1\r\n");
    e("blpop", &["wk", "1"], &arr(&["wk", "first"]));
    // Park a fresh waiter on the drained key.
    let parked = shared.clone();
    let waiter = std::thread::spawn(move || {
        let mut out = Vec::new();
        let handler = command::lookup("blpop").expect("blpop registered");
        let prefix = hash::slot_with_prefix(hash::hash_tag(b"wk")).1;
        let argv = vec![b"wk".to_vec(), b"5".to_vec()];
        let mut ctx = command::Ctx {
            shared: &parked,
            prefix_key: prefix,
            args: argv,
            out: &mut out,
            close_conn: false,
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(handler(&mut ctx));
        out
    });
    std::thread::sleep(Duration::from_millis(300));
    // This notify must reach the parked waiter, not a stale one.
    e("lpush", &["wk", "second"], b":1\r\n");
    let reply = waiter.join().expect("waiter thread");
    assert_eq!(reply, arr(&["wk", "second"]), "parked blpop must be woken");
}

/// Over the wire: a parked BLPOP on connection A is woken by an LPUSH
/// from connection B (modeled on lite's `block_wakes_on_xadd_over_wire`).
#[tokio::test]
async fn blpop_wakes_on_lpush_over_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 10).await;
    let t = TOKEN;
    let mut sock = tokio::net::TcpStream::connect(&node.resp)
        .await
        .expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Pipelined AUTH + BLPOP in one write: the blocking-dispatch flush
    // delivers the +OK before the BLPOP parks.
    let mut pipelined = frame(&["AUTH", t]);
    pipelined.extend_from_slice(&frame(&["BLPOP", "wake:key", "5000"]));
    sock.write_all(&pipelined)
        .await
        .expect("pipelined auth+blpop write");
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), sock.read_exact(&mut hello))
        .await
        .expect("+OK flushed before the BLPOP parks")
        .expect("auth reply");
    assert_eq!(&hello, b"+OK\r\n");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    let pushed = cmd_one_shot(&node.resp, t, &[b"lpush", b"wake:key", b"hello"]).await;
    assert_eq!(pushed, b":1".to_vec(), "lpush reply (line sans CRLF)");
    // The parked reader must wake (not park out the full 5s).
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(4), sock.read(&mut chunk))
            .await
            .expect("reader woken within 4s")
            .expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if contains_bytes(&buf, b"hello") {
            break;
        }
    }
    assert!(
        contains_bytes(&buf, b"*2\r\n")
            && contains_bytes(&buf, b"wake:key")
            && contains_bytes(&buf, b"hello"),
        "reply {:?}",
        buf
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "wake-up, not timeout"
    );
    // Timeouts over the wire: BRPOPLPUSH's null bulk and BLPOP's null
    // array (read_one hands back the line without its trailing CRLF).
    // src/dst share a {wg} tag: moves must hash to one slot.
    let brp = cmd_one_shot(
        &node.resp,
        t,
        &[b"brpoplpush", b"{wg}src", b"{wg}dst", b"0.2"],
    )
    .await;
    assert_eq!(brp, b"$-1".to_vec());
    let blpop_to = cmd_one_shot(&node.resp, t, &[b"blpop", b"wmissing", b"0.15"]).await;
    assert_eq!(blpop_to, b"*-1".to_vec());
}

/// Pipelined AUTH + BLPOP in ONE write: the +OK must hit the socket
/// BEFORE the BLPOP parks (blocking dispatch flushes prior replies of
/// the same read batch), instead of being held hostage for the whole
/// park. A second connection's LPUSH then wakes the parked pop.
#[tokio::test]
async fn pipelined_replies_flush_before_blpop_parks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 10).await;
    let t = TOKEN;
    let mut sock = tokio::net::TcpStream::connect(&node.resp)
        .await
        .expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Two frames, one TCP write: AUTH followed by a parking BLPOP.
    let mut pipelined = frame(&["AUTH", t]);
    pipelined.extend_from_slice(&frame(&["BLPOP", "flush:key", "5"]));
    sock.write_all(&pipelined).await.expect("pipelined write");

    // The +OK must arrive within 1s -- far short of the 5s park, so only
    // a pre-park flush can satisfy this (the old code held it back).
    let mut hello = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(1), sock.read_exact(&mut hello))
        .await
        .expect("+OK must flush before the BLPOP parks")
        .expect("read");
    assert_eq!(&hello, b"+OK\r\n");

    // The parked BLPOP still answers: an LPUSH from a second connection
    // wakes it and the *2 pair reply follows the already-flushed +OK.
    let pushed = cmd_one_shot(&node.resp, t, &[b"lpush", b"flush:key", b"v"]).await;
    assert_eq!(pushed, b":1".to_vec(), "lpush reply (line sans CRLF)");
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(4), sock.read(&mut chunk))
            .await
            .expect("reader woken within 4s")
            .expect("read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if contains_bytes(&buf, b"\r\n$1\r\nv\r\n") {
            break;
        }
    }
    assert!(
        contains_bytes(&buf, b"*2\r\n")
            && contains_bytes(&buf, b"flush:key")
            && contains_bytes(&buf, b"\r\n$1\r\nv\r\n"),
        "reply {:?}",
        buf
    );
}
