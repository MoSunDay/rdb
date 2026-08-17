//! Handler-level tests for the read-only sorted-set commands (ZCARD/
//! ZSCORE/ZMSCORE/ZCOUNT/ZRANK/ZREVRANK): same harness as `zset_tests`
//! (crate-wide store lock, registry dispatch); ports 40521+ leave room
//! for the core tests before them.

use crate::command::test_ctx;
use crate::command::Handler;
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

#[test]
fn zreads_wrongtype_and_missing() {
    let (_g, s) = shared_for("127.0.0.1:40521");
    let wrong = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec();
    call(&s, "set", &[b"str", b"v"]);
    call(&s, "hset", &[b"h", b"f", b"v"]);
    for key in [&b"str"[..], &b"h"[..]] {
        assert_eq!(call(&s, "zcard", &[key]), wrong);
        assert_eq!(call(&s, "zscore", &[key, b"m"]), wrong);
        assert_eq!(call(&s, "zmscore", &[key, b"m"]), wrong);
        assert_eq!(call(&s, "zcount", &[key, b"1", b"2"]), wrong);
        assert_eq!(call(&s, "zrank", &[key, b"m"]), wrong);
        assert_eq!(call(&s, "zrevrank", &[key, b"m", b"WITHSCORE"]), wrong);
    }
    // Missing keys read as empty, not as errors.
    assert_eq!(int_of(&call(&s, "zcard", &[b"none"])), 0);
    assert_eq!(call(&s, "zscore", &[b"none", b"m"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "zrank", &[b"none", b"m"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&s, "zrevrank", &[b"none", b"m", b"WITHSCORE"]),
        b"*-1\r\n".to_vec()
    );
}

#[test]
fn zcount_negative_and_infinite_windows() {
    let (_g, s) = shared_for("127.0.0.1:40522");
    call(
        &s,
        "zadd",
        &[b"k", b"-inf", b"a", b"-2", b"b", b"0", b"c", b"inf", b"d"],
    );
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"-inf", b"-2"])), 2);
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"-inf", b"(-inf"])), 0);
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"(0", b"inf"])), 1);
    assert_eq!(
        int_of(&call(&s, "zcount", &[b"k", b"-infinity", b"infinity"])),
        4
    );
    assert_eq!(
        call(&s, "zcount", &[b"k", b"2", b"zz"]),
        b"-ERR min or max not valid float\r\n".to_vec()
    );
}

#[test]
fn zrank_counts_equal_scores_by_member() {
    let (_g, s) = shared_for("127.0.0.1:40523");
    // Equal scores order by member bytes: a < b < c.
    call(&s, "zadd", &[b"k", b"5", b"c", b"5", b"a", b"5", b"b"]);
    assert_eq!(int_of(&call(&s, "zrank", &[b"k", b"a"])), 0);
    assert_eq!(int_of(&call(&s, "zrank", &[b"k", b"b"])), 1);
    assert_eq!(int_of(&call(&s, "zrevrank", &[b"k", b"a"])), 2);
    assert_eq!(
        call(&s, "zrevrank", &[b"k", b"b", b"withscore"]),
        b"*2\r\n:1\r\n$1\r\n5\r\n".to_vec()
    );
}
