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

/// -0.0 sorts strictly before +0.0 in the physical score index, yet the
/// two zeros compare EQUAL numerically: an INCLUSIVE zero lower bound
/// must seek from score_sortable(-0.0) or every "-0" member falls below
/// the seek and drops out of the window (regression guard).
#[test]
fn zrangebyscore_inclusive_zero_includes_negative_zero_members() {
    let (_g, s) = shared_for("127.0.0.1:40524");
    // a at -0.0, b at +0.0 (numerically equal), c at 1.
    call(&s, "zadd", &[b"k", b"-0", b"a", b"0", b"b", b"1", b"c"]);
    // Inclusive +0 min covers BOTH zeros.
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"0", b"0"]),
        b"*2\r\n$1\r\na\r\n$1\r\nb\r\n".to_vec()
    );
    // An inclusive "-0" min behaves the same.
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"-0", b"0"]),
        b"*2\r\n$1\r\na\r\n$1\r\nb\r\n".to_vec()
    );
    // An EXCLUSIVE zero min skips both zeros: (0 equals -0 numerically.
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"(0", b"1"]),
        b"*1\r\n$1\r\nc\r\n".to_vec()
    );
    // A window starting below zero still sweeps the -0.0 members.
    call(&s, "zadd", &[b"k", b"-1", b"x"]);
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"-1", b"0"]),
        b"*3\r\n$1\r\nx\r\n$1\r\na\r\n$1\r\nb\r\n".to_vec()
    );
}

/// ZCOUNT shares the window seek: an inclusive zero lower bound must
/// count the "-0" members, an exclusive one must not.
#[test]
fn zcount_zero_lower_bound_counts_negative_zero() {
    let (_g, s) = shared_for("127.0.0.1:40525");
    call(
        &s,
        "zadd",
        &[b"k", b"-1", b"x", b"-0", b"a", b"0", b"b", b"1", b"y"],
    );
    // [0, +inf]: a (-0.0), b (+0.0), y.
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"0", b"inf"])), 3);
    // [-0, 0]: both zeros only.
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"-0", b"0"])), 2);
    // (0, +inf]: neither zero.
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"(0", b"inf"])), 1);
    // [-inf, 0]: x, a, b.
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"-inf", b"0"])), 3);
}

/// ZSCORE echoes a stored -0.0 back as the bulk "-0" (the sortable
/// round-trip is bit-exact for both zeros).
#[test]
fn zscore_serializes_negative_zero_as_minus_zero() {
    let (_g, s) = shared_for("127.0.0.1:40526");
    call(&s, "zadd", &[b"k", b"-0", b"a"]);
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$2\r\n-0\r\n".to_vec());
    // And via the increment path on a fresh member.
    assert_eq!(
        call(&s, "zincrby", &[b"k2", b"-0", b"m"]),
        b"$2\r\n-0\r\n".to_vec()
    );
    assert_eq!(call(&s, "zscore", &[b"k2", b"m"]), b"$2\r\n-0\r\n".to_vec());
    // A plain +0 member serializes WITHOUT the sign: Rust prints "0".
    call(&s, "zadd", &[b"k", b"0", b"b"]);
    assert_eq!(call(&s, "zscore", &[b"k", b"b"]), b"$1\r\n0\r\n".to_vec());
}

/// ZRANGEBYLEX/ZREVRANGEBYLEX take no WITHSCORES: the option must be
/// answered with a plain syntax error (as in Redis), not swallowed,
/// while the BYSCORE twin keeps it legal.
#[test]
fn zrangebylex_family_rejects_withscores() {
    let (_g, s) = shared_for("127.0.0.1:40527");
    call(&s, "zadd", &[b"k", b"0", b"a", b"0", b"b", b"0", b"c"]);
    let err = b"-ERR syntax error\r\n".to_vec();
    assert_eq!(
        call(&s, "zrangebylex", &[b"k", b"-", b"+", b"WITHSCORES"]),
        err
    );
    assert_eq!(
        call(&s, "zrangebylex", &[b"k", b"[a", b"(d", b"withscores"]),
        err
    );
    assert_eq!(
        call(&s, "zrevrangebylex", &[b"k", b"+", b"-", b"WITHSCORES"]),
        err
    );
    // Plain lex queries still reply, and BYSCORE keeps WITHSCORES legal.
    assert_eq!(
        call(&s, "zrangebylex", &[b"k", b"-", b"+"]),
        b"*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrevrangebylex", &[b"k", b"[c", b"-"]),
        b"*3\r\n$1\r\nc\r\n$1\r\nb\r\n$1\r\na\r\n".to_vec()
    );
    assert_eq!(
        call(
            &s,
            "zrangebyscore",
            &[b"k", b"-inf", b"+inf", b"WITHSCORES"]
        ),
        b"*6\r\n$1\r\na\r\n$1\r\n0\r\n$1\r\nb\r\n$1\r\n0\r\n$1\r\nc\r\n$1\r\n0\r\n".to_vec()
    );
}

/// ZMSCORE resolves every member record BEFORE opening the array reply
/// (a failed read must one day surface as a single -ERR, never as a
/// null buried mid-array). This locks the two-pass emit's success-path
/// bytes: the header counts ARGUMENTS (duplicates included) and each
/// slot stays a score or a null in argument order.
#[test]
fn zmscore_duplicate_and_absent_members_keep_reply_shape() {
    let (_g, s) = shared_for("127.0.0.1:40528");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2.5", b"b"]);
    assert_eq!(
        call(&s, "zmscore", &[b"k", b"a", b"zz", b"a", b"b", b"a"]),
        b"*5\r\n$1\r\n1\r\n$-1\r\n$1\r\n1\r\n$3\r\n2.5\r\n$1\r\n1\r\n".to_vec()
    );
    // A zset drained by ZREM keeps the per-slot nulls.
    call(&s, "zrem", &[b"k", b"a"]);
    assert_eq!(
        call(&s, "zmscore", &[b"k", b"a", b"b"]),
        b"*2\r\n$-1\r\n$3\r\n2.5\r\n".to_vec()
    );
}
