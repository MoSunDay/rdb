//! Handler-level tests for the ZRANGE family (rank/BYSCORE/BYLEX
//! modes, REV, LIMIT, WITHSCORES, ZLEXCOUNT): same harness as
//! `zset_tests`; ports 40531+ leave room for the core/read tests.

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

fn bulks_of(reply: &[u8]) -> Vec<Vec<u8>> {
    test_reader::parse(reply)
        .iter()
        .map(test_reader::bulk)
        .collect()
}

#[test]
fn zrange_rank_mode_negatives_rev_withscores() {
    let (_g, s) = shared_for("127.0.0.1:40531");
    call(
        &s,
        "zadd",
        &[b"k", b"1", b"a", b"2", b"b", b"3", b"c", b"4", b"d"],
    );
    assert_eq!(
        call(&s, "zrange", &[b"k", b"0", b"-1"]),
        b"*4\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n$1\r\nd\r\n".to_vec()
    );
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"1", b"2"])),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    // Negative window from the back; out-of-range stops clamp.
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"-2", b"-1"])),
        vec![b"c".to_vec(), b"d".to_vec()]
    );
    assert_eq!(call(&s, "zrange", &[b"k", b"9", b"10"]), b"*0\r\n".to_vec());
    // REV flips emission order (same members).
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"0", b"1", b"REV"])),
        vec![b"d".to_vec(), b"c".to_vec()]
    );
    // WITHSCORES interleaves score bulks, flat array.
    assert_eq!(
        call(&s, "zrange", &[b"k", b"0", b"1", b"WITHSCORES"]),
        b"*4\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrange", &[b"none", b"0", b"-1"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrange", &[b"k", b"zz", b"1"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn zrange_byscore_rev_and_limit() {
    let (_g, s) = shared_for("127.0.0.1:40532");
    call(
        &s,
        "zadd",
        &[b"k", b"1", b"a", b"2", b"b", b"3", b"c", b"4", b"d"],
    );
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"2", b"3", b"BYSCORE"])),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    // Exclusive bounds; under REV the arguments arrive as (max, min).
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"(1", b"(4", b"BYSCORE"])),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(&s, "zrange", &[b"k", b"4", b"2", b"BYSCORE", b"REV"])),
        vec![b"d".to_vec(), b"c".to_vec(), b"b".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrange",
            &[b"k", b"1", b"4", b"BYSCORE", b"LIMIT", b"1", b"2"]
        )),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    // LIMIT offsets count in reply order (reversed under REV).
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrange",
            &[b"k", b"4", b"1", b"BYSCORE", b"REV", b"LIMIT", b"1", b"2"]
        )),
        vec![b"c".to_vec(), b"b".to_vec()]
    );
    // Negative count = the rest; offsets past the end are empty.
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrange",
            &[b"k", b"1", b"4", b"BYSCORE", b"LIMIT", b"3", b"-1"]
        )),
        vec![b"d".to_vec()]
    );
    assert_eq!(
        call(
            &s,
            "zrange",
            &[b"k", b"1", b"4", b"BYSCORE", b"LIMIT", b"9", b"2"]
        ),
        b"*0\r\n".to_vec()
    );
}

#[test]
fn zrangebyscore_and_zrevrangebyscore() {
    let (_g, s) = shared_for("127.0.0.1:40533");
    call(
        &s,
        "zadd",
        &[b"k", b"1", b"a", b"2", b"b", b"3", b"c", b"4", b"d"],
    );
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrangebyscore",
            &[b"k", b"2", b"4", b"LIMIT", b"1", b"2"]
        )),
        vec![b"c".to_vec(), b"d".to_vec()]
    );
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"(2", b"3", b"WITHSCORES"]),
        b"*2\r\n$1\r\nc\r\n$1\r\n3\r\n".to_vec()
    );
    // ZREVRANGEBYSCORE takes (max, min) and emits descending.
    assert_eq!(
        bulks_of(&call(&s, "zrevrangebyscore", &[b"k", b"3", b"1"])),
        vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrevrangebyscore",
            &[b"k", b"+inf", b"-inf", b"LIMIT", b"0", b"2"]
        )),
        vec![b"d".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        call(&s, "zrangebyscore", &[b"none", b"1", b"2"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"zz", b"2"]),
        b"-ERR min or max not valid float\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrangebyscore", &[b"k", b"1", b"2", b"REV"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

#[test]
fn zrangebylex_zrevrangebylex_zlexcount() {
    let (_g, s) = shared_for("127.0.0.1:40534");
    call(
        &s,
        "zadd",
        &[b"k", b"0", b"a", b"0", b"b", b"0", b"c", b"0", b"d"],
    );
    assert_eq!(
        bulks_of(&call(&s, "zrangebylex", &[b"k", b"[a", b"[c"])),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(&s, "zrangebylex", &[b"k", b"(a", b"(d"])),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrangebylex",
            &[b"k", b"-", b"+", b"LIMIT", b"1", b"2"]
        )),
        vec![b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(&s, "zrevrangebylex", &[b"k", b"[d", b"[b"])),
        vec![b"d".to_vec(), b"c".to_vec(), b"b".to_vec()]
    );
    assert_eq!(
        bulks_of(&call(
            &s,
            "zrevrangebylex",
            &[b"k", b"+", b"-", b"LIMIT", b"0", b"2"]
        )),
        vec![b"d".to_vec(), b"c".to_vec()]
    );
    assert_eq!(
        call(&s, "zlexcount", &[b"k", b"[a", b"[c"]),
        b":3\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zlexcount", &[b"k", b"(b", b"+"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zlexcount", &[b"k", b"[c", b"[a"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zlexcount", &[b"none", b"-", b"+"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrangebylex", &[b"k", b"a", b"c"]),
        b"-ERR min or max not valid string range item\r\n".to_vec()
    );
}

#[test]
fn zrange_option_errors() {
    let (_g, s) = shared_for("127.0.0.1:40535");
    call(&s, "zadd", &[b"k", b"1", b"a"]);
    // LIMIT needs BYSCORE or BYLEX.
    assert_eq!(
        call(&s, "zrange", &[b"k", b"0", b"-1", b"LIMIT", b"0", b"1"]),
        b"-ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX\r\n".to_vec()
    );
    // WITHSCORES is meaningless with BYLEX.
    assert_eq!(
        call(&s, "zrange", &[b"k", b"-", b"+", b"BYLEX", b"WITHSCORES"]),
        b"-ERR syntax error, WITHSCORES not supported in combination with BYLEX\r\n".to_vec()
    );
    // Unknown options and BYSCORE+BYLEX conflict.
    assert_eq!(
        call(&s, "zrange", &[b"k", b"0", b"-1", b"NOPE"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrange", &[b"k", b"1", b"2", b"BYSCORE", b"BYLEX"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrangebylex", &[b"k", b"-", b"+", b"LIMIT", b"1"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

/// Regression: exclusive score bounds around ±0.0. Sortable order places
/// -0.0 strictly BEFORE +0.0 while IEEE equality collapses the two, so
/// IEEE-based window tests skipped +0.0 members below an exclusive -0.0
/// min (and admitted -0.0 past an exclusive +0.0 max).
#[test]
fn zrangebyscore_exclusive_bounds_and_signed_zero() {
    let (_g, s) = shared_for("127.0.0.1:40536");
    call(
        &s,
        "zadd",
        &[
            b"k", b"-1.5", b"neg", b"-0", b"mzero", b"0", b"pzero", b"1.5", b"pos",
        ],
    );
    // Exclusive -0.0 min: the +0.0 member is ABOVE the bound, not equal.
    assert_eq!(
        bulks_of(&call(&s, "zrangebyscore", &[b"k", b"(-0.0", b"+inf"])),
        vec![b"pzero".to_vec(), b"pos".to_vec()]
    );
    // Inclusive -0.0 min + exclusive +0.0 max: only the -0.0 member.
    assert_eq!(
        bulks_of(&call(&s, "zrangebyscore", &[b"k", b"-0.0", b"(0.0"])),
        vec![b"mzero".to_vec()]
    );
    // Strictly-between-±0.0 window is empty: nothing removed, all stay.
    assert_eq!(
        call(&s, "zremrangebyscore", &[b"k", b"(-0.0", b"(+0.0"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        bulks_of(&call(&s, "zrangebyscore", &[b"k", b"-inf", b"+inf"])),
        vec![
            b"neg".to_vec(),
            b"mzero".to_vec(),
            b"pzero".to_vec(),
            b"pos".to_vec()
        ]
    );
}
