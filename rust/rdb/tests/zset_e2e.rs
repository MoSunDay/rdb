//! End-to-end tests for the sorted-set family: in-process lifecycle
//! through the real command registry (ZADD flags, reads, ranges by
//! score/lex, removals, ZSCAN paging, ZUNIONSTORE algebra, TTL interplay,
//! blocking pops) plus an over-the-wire BZPOPMIN wake-up against a real
//! spawned node. `expect` asserts exact RESP reply bytes; `bulks` decodes
//! a flat array of bulks for cursor loops.

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
    let dir = std::env::temp_dir().join(format!("rdb-zs-e2e-{}-{tag}", std::process::id()));
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

/// Payloads of a flat RESP array of bulks (the ZSCAN reply shape).
fn bulks(reply: &[u8]) -> Vec<Vec<u8>> {
    let mut rest = reply;
    let mut out = Vec::new();
    if rest.starts_with(b"*") {
        let hdr = rest.iter().position(|&b| b == b'\n').expect("array header");
        rest = &rest[hdr + 1..];
    }
    while rest.starts_with(b"$") {
        let end = match rest.iter().position(|&b| b == b'\n') {
            Some(p) => p,
            None => break,
        };
        let Ok(len) = std::str::from_utf8(&rest[1..end - 1])
            .expect("bulk header")
            .parse::<usize>()
        else {
            break; // `$-1`: null bulk ends the walk
        };
        if rest.len() < end + 1 + len {
            break;
        }
        out.push(rest[end + 1..end + 1 + len].to_vec());
        rest = &rest[(end + 1 + len + 2).min(rest.len())..];
    }
    out
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
fn zset_lifecycle_score_rank_range_popmin() {
    let shared = shared_for("45001");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e(
        "zadd",
        &["z", "1", "a", "2", "b", "3", "c", "3", "d"],
        b":4\r\n",
    );
    e("type", &["z"], b"+zset\r\n");
    // Ties at score 3 order by member bytes: c before d.
    e("zscore", &["z", "c"], b"$1\r\n3\r\n");
    e("zscore", &["z", "zz"], b"$-1\r\n");
    e("zrank", &["z", "a"], b":0\r\n");
    e("zrank", &["z", "d"], b":3\r\n");
    e("zrank", &["z", "zz"], b"$-1\r\n");
    e("zrange", &["z", "0", "-1"], &arr(&["a", "b", "c", "d"]));
    e("zrange", &["z", "0", "-1", "WITHSCORES"], &arr(RANKED));
    // ZPOPMIN takes the lowest score (member bytes break ties).
    e("zpopmin", &["z"], &arr(&["a", "1"]));
    e("zcard", &["z"], b":3\r\n");
    e("zrange", &["none", "0", "-1"], b"*0\r\n");
}

const RANKED: &[&str] = &["a", "1", "b", "2", "c", "3", "d", "3"];

#[test]
fn zadd_gt_ch_matrix_and_zincrby() {
    let shared = shared_for("45002");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("zadd", &["m", "2", "a"], b":1\r\n");
    // CH counts score updates as well as additions.
    e("zadd", &["m", "GT", "CH", "3", "a"], b":1\r\n");
    e("zscore", &["m", "a"], b"$1\r\n3\r\n");
    // Without CH a raising update is applied but not counted.
    e("zadd", &["m", "GT", "9", "a"], b":0\r\n");
    e("zscore", &["m", "a"], b"$1\r\n9\r\n");
    // GT never lowers; a no-op update stays uncounted under CH.
    e("zadd", &["m", "GT", "1", "a"], b":0\r\n");
    e("zscore", &["m", "a"], b"$1\r\n9\r\n");
    e("zadd", &["m", "GT", "CH", "9", "a"], b":0\r\n");
    // GT skips missing members entirely.
    e("zadd", &["m", "GT", "5", "zz"], b":0\r\n");
    e("zscore", &["m", "zz"], b"$-1\r\n");
    // ZINCRBY applies on top and formats the resulting score.
    e("zincrby", &["m", "1.5", "a"], b"$4\r\n10.5\r\n");
    e("zincrby", &["fresh", "1.5", "n"], b"$3\r\n1.5\r\n");
}

#[test]
fn zrangebyscore_bylex_and_zremrangebyrank() {
    let shared = shared_for("45003");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e(
        "zadd",
        &["q", "1", "a", "2", "b", "3", "c", "4", "d"],
        b":4\r\n",
    );
    // Exclusive lower bound `(1`, inclusive upper `3`.
    e("zrangebyscore", &["q", "(1", "3"], &arr(&["b", "c"]));
    // BYLEX needs equal scores; `[a`..`[c` is inclusive on both ends.
    e(
        "zadd",
        &["l", "0", "a", "0", "b", "0", "c", "0", "d"],
        b":4\r\n",
    );
    e("zrangebylex", &["l", "[a", "[c"], &arr(&["a", "b", "c"]));
    // Rank removal drops the two lowest-scored members.
    e("zremrangebyrank", &["q", "0", "1"], b":2\r\n");
    e("zrange", &["q", "0", "-1"], &arr(&["c", "d"]));
    // Draining the zset deletes the key outright.
    e("zremrangebyrank", &["q", "0", "-1"], b":2\r\n");
    e("exists", &["q"], b":0\r\n");
}

#[test]
fn zscan_cursor_loop_pages_of_one() {
    let shared = shared_for("45004");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e(
        "zadd",
        &["s", "1", "a", "2", "b", "3", "c", "4", "d"],
        b":4\r\n",
    );
    // Walk COUNT 1 pages: four member pages, then a terminal page that
    // carries only the reset cursor "0" (a page of one ends iteration).
    let mut cursor = b"0".to_vec();
    let mut members: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;
    loop {
        let c = text(&cursor);
        let reply = call(&shared, "zscan", &["s", &c, "COUNT", "1"]);
        let page = bulks(&reply);
        assert!(!page.is_empty(), "cursor always present: {reply:?}");
        cursor = page[0].clone();
        match page.len() {
            1 => assert_eq!(cursor, b"0", "member-less page must reset: {reply:?}"),
            2 => members.push(page[1].clone()),
            n => panic!("COUNT 1 pages carry at most one member, got {n}: {reply:?}"),
        }
        pages += 1;
        if cursor == b"0" {
            break;
        }
        assert!(pages < 32, "cursor loop did not terminate: {reply:?}");
    }
    assert_eq!(pages, 5, "four member pages plus the cursor reset");
    members.sort();
    let want = [b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()];
    assert_eq!(members, want);
    // WITHSCORES interleaves a score bulk after each member; the cursor
    // is the hex of the LAST member returned ("a" -> 61).
    e(
        "zscan",
        &["s", "0", "COUNT", "1", "WITHSCORES"],
        &arr(&["61", "a", "1"]),
    );
}

#[test]
fn zunionstore_weights_aggregate_overwrite_crossslot() {
    let shared = shared_for("45005");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("zadd", &["{u}1", "1", "a", "2", "b"], b":2\r\n");
    e("zadd", &["{u}2", "10", "a", "20", "c"], b":2\r\n");
    // Fresh destination: SUM by default (1 + 10, 2, 20).
    e("zunionstore", &["{u}d", "2", "{u}1", "{u}2"], b":3\r\n");
    e("zscore", &["{u}d", "a"], b"$2\r\n11\r\n");
    e("zscore", &["{u}d", "c"], b"$2\r\n20\r\n");
    // WEIGHTS 2 1: a = 1*2 + 10*1.
    e(
        "zunionstore",
        &["{u}w", "2", "{u}1", "{u}2", "WEIGHTS", "2", "1"],
        b":3\r\n",
    );
    e("zscore", &["{u}w", "a"], b"$2\r\n12\r\n");
    // AGGREGATE MIN / MAX over the same inputs.
    e(
        "zunionstore",
        &["{u}m", "2", "{u}1", "{u}2", "AGGREGATE", "MIN"],
        b":3\r\n",
    );
    e("zscore", &["{u}m", "a"], b"$1\r\n1\r\n");
    e(
        "zunionstore",
        &["{u}x", "2", "{u}1", "{u}2", "AGGREGATE", "MAX"],
        b":3\r\n",
    );
    e("zscore", &["{u}x", "a"], b"$2\r\n10\r\n");
    // Overwriting an existing destination replaces its members whole.
    e("zadd", &["{u}d", "1", "stale"], b":1\r\n");
    e("zunionstore", &["{u}d", "2", "{u}1", "{u}2"], b":3\r\n");
    e("zscore", &["{u}d", "stale"], b"$-1\r\n");
    e("zcard", &["{u}d"], b":3\r\n");
    // Different slots are rejected before any mutation (exact bytes).
    let far = call(&shared, "zunionstore", &["{u}d", "2", "{u}1", "other"]);
    assert_eq!(far, CROSSSLOT.to_vec(), "in 'zunionstore'");
}

#[test]
fn zset_ttl_lazy_purge_on_typed_read() {
    let shared = shared_for("45006");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("zadd", &["{t}zk", "1", "a", "2", "b"], b":2\r\n");
    e("expire", &["{t}zk", "60"], b":1\r\n");
    let ttl = text(&call(&shared, "ttl", &["{t}zk"]));
    assert!(ttl == ":60\r\n" || ttl == ":59\r\n", "ttl {ttl}");
    e("zcard", &["{t}zk"], b":2\r\n");
    // A short real deadline: the typed read after it lazily purges meta,
    // member and score-index records together.
    e("pexpire", &["{t}zk", "100"], b":1\r\n");
    std::thread::sleep(Duration::from_millis(250));
    e("zcard", &["{t}zk"], b":0\r\n");
    e("exists", &["{t}zk"], b":0\r\n");
    e("zscore", &["{t}zk", "a"], b"$-1\r\n");
    let keys = text(&call(&shared, "keys", &["*"]));
    assert!(!keys.contains("{t}zk"), "purged key still listed: {keys}");
    e("del", &["{t}zk"], b":0\r\n");
}

#[test]
fn bzpopmin_immediate_and_timeout() {
    let shared = shared_for("45007");
    let e = |n: &str, a: &[&str], w: &[u8]| expect(&shared, n, a, w);
    e("zadd", &["bk", "1.5", "one"], b":1\r\n");
    // Immediate hit on the latched quick path: *3 (key, member, score).
    e("bzpopmin", &["bk", "1"], &arr(&["bk", "one", "1.5"]));
    // The drained key now times out with the null array (~100ms parked).
    let started = Instant::now();
    e("bzpopmin", &["bk", "0.1"], b"*-1\r\n");
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

/// Over the wire: a parked BZPOPMIN on connection A is woken by a ZADD
/// from connection B (modeled on lite's `block_wakes_on_xadd_over_wire`).
#[tokio::test]
async fn bzpopmin_wakes_on_zadd_over_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut node = spawn_node(dir.path(), 0, true, None);
    wait_resp_ready(&mut node, 10).await;
    let t = TOKEN;
    let mut sock = tokio::net::TcpStream::connect(&node.resp)
        .await
        .expect("connect");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // AUTH separately: replies flush per read-batch, so a pipelined
    // BZPOPMIN would hold the +OK hostage until it stops parking.
    sock.write_all(&frame(&["AUTH", t]))
        .await
        .expect("auth write");
    let mut hello = [0u8; 5];
    sock.read_exact(&mut hello).await.expect("auth reply");
    assert_eq!(&hello, b"+OK\r\n");
    sock.write_all(&frame(&["BZPOPMIN", "wake:z", "5000"]))
        .await
        .expect("bzpopmin write");
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = Instant::now();
    let added = cmd_one_shot(&node.resp, t, &[b"zadd", b"wake:z", b"1.5", b"one"]).await;
    assert_eq!(added, b":1".to_vec(), "zadd reply (line sans CRLF)");
    // The parked reader must wake with the flat *3 triple.
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
        if contains_bytes(&buf, b"one") {
            break;
        }
    }
    assert!(
        contains_bytes(&buf, b"*3\r\n")
            && contains_bytes(&buf, b"wake:z")
            && contains_bytes(&buf, b"one")
            && contains_bytes(&buf, b"1.5"),
        "reply {:?}",
        buf
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "wake-up, not timeout"
    );
}
