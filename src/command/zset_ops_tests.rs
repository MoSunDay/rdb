//! Handler-level tests for the P2-C2 zset additions: range removals
//! (ZREMRANGEBY*), ZSCAN, the Z*STORE algebra and the blocking pops.
//! Same harness as `zset_tests`; ports 40600+ (405xx are taken).

use crate::command::test_ctx;
use crate::command::Handler;
use crate::resp::codec::test_reader;
use crate::state::{testutil, Shared};

const PREFIX: &[u8] = b"70/";

fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
    let guard = crate::command::string::TEST_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conf = testutil::test_config();
    conf.bind = bind.to_string();
    (guard, testutil::shared_with(conf))
}

fn call(shared: &Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
    let handler: Handler =
        crate::command::lookup(name).unwrap_or_else(|| panic!("{name} registered"));
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(handler(&mut ctx));
    out
}

fn int_of(reply: &[u8]) -> i64 {
    let text = String::from_utf8(reply.to_vec()).unwrap();
    text.trim_start_matches(':').trim_end().parse().unwrap()
}

fn bulks_of(reply: &[u8]) -> Vec<Vec<u8>> {
    test_reader::parse(reply)
        .iter()
        .map(test_reader::bulk)
        .collect()
}

const WRONGTYPE_REPLY: &[u8] =
    b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";

fn zadd4(shared: &Shared, key: &[u8]) {
    call(
        shared,
        "zadd",
        &[key, b"1", b"a", b"2", b"b", b"3", b"c", b"4", b"d"],
    );
}

#[test]
fn zremrangebyrank_window_negatives_and_drain() {
    let (_g, s) = shared_for("127.0.0.1:40601");
    zadd4(&s, b"k");
    assert_eq!(
        int_of(&call(&s, "zremrangebyrank", &[b"k", b"1", b"-2"])),
        2
    );
    assert_eq!(
        int_of(&call(&s, "zremrangebyrank", &[b"k", b"9", b"10"])),
        0
    );
    assert_eq!(int_of(&call(&s, "zremrangebyrank", &[b"k", b"2", b"1"])), 0);
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"0", b"-1"])),
        vec![b"a".to_vec(), b"d".to_vec()]
    );
    // Draining to empty deletes the key entirely.
    assert_eq!(
        int_of(&call(&s, "zremrangebyrank", &[b"k", b"0", b"-1"])),
        2
    );
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 0);
    assert_eq!(
        int_of(&call(&s, "zremrangebyrank", &[b"none", b"0", b"1"])),
        0
    );
    assert_eq!(
        call(&s, "zremrangebyrank", &[b"k", b"x", b"1"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn zremrangebyscore_exclusive_bounds() {
    let (_g, s) = shared_for("127.0.0.1:40602");
    zadd4(&s, b"k");
    assert_eq!(
        int_of(&call(&s, "zremrangebyscore", &[b"k", b"(1", b"3"])),
        2
    );
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"0", b"-1"])),
        vec![b"a".to_vec(), b"d".to_vec()]
    );
    assert_eq!(
        int_of(&call(&s, "zremrangebyscore", &[b"k", b"-inf", b"+inf"])),
        2
    );
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 0);
    assert_eq!(
        call(&s, "zremrangebyscore", &[b"k", b"zz", b"3"]),
        b"-ERR min or max not valid float\r\n".to_vec()
    );
}

/// Regression: an INCLUSIVE zero min must reach `-0.0` members -- the
/// physical seek starts at sortable(-0.0), not sortable(+0.0), or every
/// negative-zero member sorts below the scan and dodges the removal.
#[test]
fn zremrangebyscore_inclusive_zero_min_removes_negative_zero_members() {
    let (_g, s) = shared_for("127.0.0.1:40699");
    call(
        &s,
        "zadd",
        &[b"k", b"-0", b"neg", b"0", b"pos", b"1", b"one"],
    );
    assert_eq!(
        int_of(&call(&s, "zremrangebyscore", &[b"k", b"0", b"0.5"])),
        2,
        "-0.0 and +0.0 members both fall under the inclusive 0 min"
    );
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"0", b"-1"])),
        vec![b"one".to_vec()]
    );
}

#[test]
fn zremrangebylex_inclusive_bounds() {
    let (_g, s) = shared_for("127.0.0.1:40603");
    call(
        &s,
        "zadd",
        &[b"k", b"0", b"a", b"0", b"b", b"0", b"c", b"0", b"d"],
    );
    assert_eq!(
        int_of(&call(&s, "zremrangebylex", &[b"k", b"[a", b"[c"])),
        3
    );
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"0", b"-1"])),
        vec![b"d".to_vec()]
    );
    assert_eq!(int_of(&call(&s, "zremrangebylex", &[b"k", b"-", b"+"])), 1);
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 0);
    assert_eq!(
        call(&s, "zremrangebylex", &[b"k", b"a", b"c"]),
        b"-ERR min or max not valid string range item\r\n".to_vec()
    );
}

#[test]
fn zscan_full_cursor_loop_and_match() {
    let (_g, s) = shared_for("127.0.0.1:40604");
    zadd4(&s, b"k");
    // Walk COUNT 2 pages until the cursor returns to "0".
    let mut cursor = b"0".to_vec();
    let mut all: Vec<Vec<u8>> = Vec::new();
    loop {
        let reply = call(&s, "zscan", &[b"k", &cursor, b"COUNT", b"2"]);
        let page = bulks_of(&reply);
        cursor = page[0].clone();
        all.extend(page[1..].iter().cloned());
        if cursor == b"0" {
            break;
        }
    }
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // MATCH filters inside the page; a fully matched page ends at "0".
    let matched = bulks_of(&call(&s, "zscan", &[b"k", b"0", b"MATCH", b"a*"]));
    assert_eq!(matched, vec![b"0".to_vec(), b"a".to_vec()]);
}

#[test]
fn zscan_withscores_interleave_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40605");
    zadd4(&s, b"k");
    assert_eq!(
        call(&s, "zscan", &[b"k", b"0", b"COUNT", b"2", b"WITHSCORES"]),
        b"*5\r\n$2\r\n62\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zscan", &[b"k", b"zz"]),
        b"-ERR invalid cursor\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zscan", &[b"k", b"0", b"COUNT", b"0"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zscan", &[b"k", b"0", b"NOPE"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    call(&s, "set", &[b"str", b"v"]);
    assert_eq!(call(&s, "zscan", &[b"str", b"0"]), WRONGTYPE_REPLY.to_vec());
}

#[test]
fn zunionstore_sum_weights_aggregate() {
    let (_g, s) = shared_for("127.0.0.1:40606");
    call(&s, "zadd", &[b"{t}1", b"1", b"a", b"2", b"b"]);
    call(&s, "zadd", &[b"{t}2", b"10", b"a", b"20", b"c"]);
    assert_eq!(
        int_of(&call(&s, "zunionstore", &[b"{t}d", b"2", b"{t}1", b"{t}2"])),
        3
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}d", b"a"]),
        b"$2\r\n11\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}d", b"c"]),
        b"$2\r\n20\r\n".to_vec()
    );
    assert_eq!(
        int_of(&call(
            &s,
            "zunionstore",
            &[b"{t}w", b"2", b"{t}1", b"{t}2", b"WEIGHTS", b"2", b"1"]
        )),
        3
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}w", b"a"]),
        b"$2\r\n12\r\n".to_vec()
    );
    assert_eq!(
        int_of(&call(
            &s,
            "zunionstore",
            &[b"{t}m", b"2", b"{t}1", b"{t}2", b"AGGREGATE", b"MIN"]
        )),
        3
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}m", b"a"]),
        b"$1\r\n1\r\n".to_vec()
    );
    assert_eq!(
        int_of(&call(
            &s,
            "zunionstore",
            &[b"{t}x", b"2", b"{t}1", b"{t}2", b"AGGREGATE", b"MAX"]
        )),
        3
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}x", b"a"]),
        b"$2\r\n10\r\n".to_vec()
    );
}

#[test]
fn zunionstore_error_replies() {
    let (_g, s) = shared_for("127.0.0.1:40607");
    assert_eq!(
        call(&s, "zunionstore", &[b"d", b"0"]),
        b"-ERR at least 1 input key is needed for 'zunionstore' command\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zinterstore", &[b"d", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zunionstore", &[b"d", b"3", b"k1", b"k2"]),
        b"-ERR syntax error, wrong number of keys\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "zunionstore",
            &[b"{t}d", b"2", b"{t}1", b"{t}2", b"WEIGHTS", b"1"]
        ),
        b"-ERR WEIGHTS options doesn't match the number of keys\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "zunionstore",
            &[b"{t}d", b"1", b"{t}1", b"WEIGHTS", b"nan"]
        ),
        b"-ERR weight value is not a float\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "zunionstore",
            &[b"{t}d", b"1", b"{t}1", b"AGGREGATE", b"AVG"]
        ),
        b"-ERR syntax error\r\n".to_vec()
    );
    // inf * 0 under SUM: the resulting score is not a number.
    call(&s, "zadd", &[b"{t}i", b"inf", b"a"]);
    assert_eq!(
        call(
            &s,
            "zunionstore",
            &[b"{t}d", b"1", b"{t}i", b"WEIGHTS", b"0"]
        ),
        b"-ERR resulting score is not a number\r\n".to_vec()
    );
}

#[test]
fn zunionstore_dest_overwrite_and_crossslot() {
    let (_g, s) = shared_for("127.0.0.1:40608");
    call(&s, "zadd", &[b"{t}d", b"1", b"old1", b"2", b"old2"]);
    call(&s, "zadd", &[b"{t}1", b"5", b"new"]);
    assert_eq!(
        int_of(&call(&s, "zunionstore", &[b"{t}d", b"1", b"{t}1"])),
        1
    );
    assert_eq!(int_of(&call(&s, "zcard", &[b"{t}d"])), 1);
    assert_eq!(call(&s, "zscore", &[b"{t}d", b"old1"]), b"$-1\r\n".to_vec());
    // A string destination may not be silently replaced.
    call(&s, "set", &[b"{t}s", b"v"]);
    assert_eq!(
        call(&s, "zunionstore", &[b"{t}s", b"1", b"{t}1"]),
        WRONGTYPE_REPLY.to_vec()
    );
    // Keys in different slots are rejected before anything is touched.
    assert_eq!(
        call(&s, "zunionstore", &[b"{t}d", b"2", b"{t}1", b"other"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
}

#[test]
fn zinterstore_and_zdiffstore() {
    let (_g, s) = shared_for("127.0.0.1:40609");
    call(&s, "zadd", &[b"{t}1", b"1", b"a", b"2", b"b"]);
    call(&s, "zadd", &[b"{t}2", b"10", b"b", b"20", b"c"]);
    assert_eq!(
        int_of(&call(&s, "zinterstore", &[b"{t}i", b"2", b"{t}1", b"{t}2"])),
        1
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}i", b"b"]),
        b"$2\r\n12\r\n".to_vec()
    );
    assert_eq!(
        int_of(&call(&s, "zdiffstore", &[b"{t}x", b"2", b"{t}1", b"{t}2"])),
        1
    );
    assert_eq!(
        call(&s, "zscore", &[b"{t}x", b"a"]),
        b"$1\r\n1\r\n".to_vec()
    );
    // Empty result deletes the destination outright ({t}3 is disjoint
    // from {t}1).
    call(&s, "zadd", &[b"{t}3", b"5", b"e"]);
    call(&s, "zadd", &[b"{t}e", b"1", b"z"]);
    assert_eq!(
        int_of(&call(&s, "zinterstore", &[b"{t}e", b"2", b"{t}1", b"{t}3"])),
        0
    );
    assert_eq!(int_of(&call(&s, "zcard", &[b"{t}e"])), 0);
}

#[test]
fn bzpopmin_immediate_and_timeout() {
    let (_g, s) = shared_for("127.0.0.1:40610");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2", b"b"]);
    assert_eq!(
        call(&s, "bzpopmin", &[b"k", b"0.1"]),
        b"*3\r\n$1\r\nk\r\n$1\r\na\r\n$1\r\n1\r\n".to_vec()
    );
    // Two same-slot keys, first empty: the pop lands on the second.
    call(&s, "zadd", &[b"{p}b", b"9", b"m"]);
    assert_eq!(
        call(&s, "bzpopmin", &[b"{p}a", b"{p}b", b"0.1"]),
        b"*3\r\n$4\r\n{p}b\r\n$1\r\nm\r\n$1\r\n9\r\n".to_vec()
    );
    // The original key still holds "b"; drain it, then park out the
    // 100ms deadline on the empty one.
    assert_eq!(
        call(&s, "bzpopmin", &[b"k", b"0.1"]),
        b"*3\r\n$1\r\nk\r\n$1\r\nb\r\n$1\r\n2\r\n".to_vec()
    );
    assert_eq!(call(&s, "bzpopmin", &[b"k", b"0.1"]), b"*-1\r\n".to_vec());
    assert_eq!(
        call(&s, "bzpopmin", &[b"k", b"abc"]),
        b"-ERR timeout is not a float or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "bzpopmin", &[b"k"]),
        b"-ERR wrong number of arguments for 'bzpopmin' command\r\n".to_vec()
    );
}

#[test]
fn bzpopmax_immediate_and_wrongtype() {
    let (_g, s) = shared_for("127.0.0.1:40611");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2.5", b"b"]);
    assert_eq!(
        call(&s, "bzpopmax", &[b"k", b"1"]),
        b"*3\r\n$1\r\nk\r\n$1\r\nb\r\n$3\r\n2.5\r\n".to_vec()
    );
    // A blocking pop on a wrong-typed key errors immediately.
    call(&s, "set", &[b"str", b"v"]);
    assert_eq!(
        call(&s, "bzpopmin", &[b"str", b"0.1"]),
        WRONGTYPE_REPLY.to_vec()
    );
    assert_eq!(
        call(&s, "bzpopmax", &[b"{x}a", b"{y}b", b"1"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
}
