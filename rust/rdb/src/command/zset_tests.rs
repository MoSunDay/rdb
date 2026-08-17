//! Handler-level tests for the sorted-set core (ZADD/ZINCRBY plus
//! ZREM/ZPOPMIN/ZPOPMAX, ZSCORE-family spot checks): crate-wide store
//! lock, fresh `Shared` per test, dispatch through the registry so the
//! registered names are exercised too. Ports 40501+ (404xx are taken by
//! the list tests). Range-family tests live in `zset_range_tests`.

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

/// Decode a single bulk reply (`$len\r\n<bytes>\r\n`); the test_reader
/// only parses arrays, so decode the frame by hand.
fn bulk_of(reply: &[u8]) -> Vec<u8> {
    let text_end = reply.iter().position(|&b| b == b'\n').expect("line ends");
    let len: usize = std::str::from_utf8(&reply[1..text_end - 1])
        .expect("bulk header")
        .parse()
        .expect("bulk length");
    reply[text_end + 1..text_end + 1 + len].to_vec()
}

fn bulks_of(reply: &[u8]) -> Vec<Vec<u8>> {
    test_reader::parse(reply)
        .iter()
        .map(test_reader::bulk)
        .collect()
}

fn zadd3(shared: &Shared, key: &[u8], a: &[u8], sa: &[u8], b: &[u8], sb: &[u8]) -> Vec<u8> {
    call(shared, "zadd", &[key, sa, a, sb, b])
}

#[test]
fn zadd_adds_and_reports_new_members() {
    let (_g, s) = shared_for("127.0.0.1:40501");
    assert_eq!(
        call(&s, "zadd", &[b"k"]),
        b"-ERR wrong number of arguments for 'zadd' command\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"1", b"a", b"2"]),
        b"-ERR wrong number of arguments for 'zadd' command\r\n".to_vec()
    );
    assert_eq!(int_of(&zadd3(&s, b"k", b"a", b"3.5", b"b", b"1")), 2);
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 2);
    // Re-adding the same members adds nothing.
    assert_eq!(int_of(&zadd3(&s, b"k", b"a", b"9", b"b", b"9")), 0);
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 2);
    assert_eq!(
        call(&s, "zscore", &[b"k", b"a"]),
        b"$1\r\n9\r\n".to_vec() // the second ZADD rewrote a's score
    );
    // A bad score is rejected before anything is written.
    assert_eq!(
        call(&s, "zadd", &[b"k", b"nan", b"c"]),
        b"-ERR value is not a valid float\r\n".to_vec()
    );
}

#[test]
fn zadd_wrongtype_on_string_and_hash_keys() {
    let (_g, s) = shared_for("127.0.0.1:40502");
    let wrong = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec();
    call(&s, "set", &[b"str", b"v"]);
    assert_eq!(call(&s, "zadd", &[b"str", b"1", b"a"]), wrong);
    assert_eq!(call(&s, "zrange", &[b"str", b"0", b"-1"]), wrong);
    call(&s, "hset", &[b"h", b"f", b"v"]);
    assert_eq!(call(&s, "zadd", &[b"h", b"1", b"a"]), wrong);
    assert_eq!(call(&s, "zrange", &[b"h", b"0", b"-1"]), wrong);
}

#[test]
fn zadd_nx_xx_gt_lt_flags() {
    let (_g, s) = shared_for("127.0.0.1:40503");
    zadd3(&s, b"k", b"a", b"1", b"b", b"2");
    // NX never touches existing members.
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"nx", b"9", b"a"])), 0);
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$1\r\n1\r\n".to_vec());
    // NX still creates missing ones.
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"NX", b"5", b"c"])), 1);
    // XX never creates; on a missing key nothing is written at all.
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"absent", b"xx", b"1", b"a"])),
        0
    );
    assert_eq!(int_of(&call(&s, "exists", &[b"absent"])), 0);
    // GT only raises scores (missing members are skipped), CH counts updates.
    assert_eq!(
        int_of(&call(
            &s,
            "zadd",
            &[b"k", b"GT", b"CH", b"3", b"a", b"1", b"zz"]
        )),
        1
    );
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$1\r\n3\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"gt", b"2", b"a"])), 0);
    // LT only lowers scores.
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"k", b"LT", b"CH", b"1", b"a"])),
        1
    );
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"lt", b"9", b"a"])), 0);
}

#[test]
fn zadd_option_conflicts_and_incr_arity() {
    let (_g, s) = shared_for("127.0.0.1:40504");
    assert_eq!(
        call(&s, "zadd", &[b"k", b"NX", b"XX", b"1", b"a"]),
        b"-ERR XX and NX options at the same time are not compatible\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"NX", b"GT", b"1", b"a"]),
        b"-ERR GT, LT, and/or NX options at the same time are not compatible\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"GT", b"LT", b"1", b"a"]),
        b"-ERR GT, LT, and/or NX options at the same time are not compatible\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"INCR", b"1", b"a", b"2", b"b"]),
        b"-ERR INCR option supports a single increment-element pair\r\n".to_vec()
    );
}

#[test]
fn zadd_incr_bulk_and_null_replies() {
    let (_g, s) = shared_for("127.0.0.1:40505");
    assert_eq!(
        call(&s, "zadd", &[b"k", b"INCR", b"1.5", b"a"]),
        b"$3\r\n1.5\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"incr", b"2", b"a"]),
        b"$3\r\n3.5\r\n".to_vec()
    );
    // NX vetoes the update of an existing member: null bulk.
    assert_eq!(
        call(&s, "zadd", &[b"k", b"INCR"]),
        b"-ERR wrong number of arguments for 'zadd' command\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"NX", b"INCR", b"1", b"a"]),
        b"$-1\r\n".to_vec()
    );
    // GT vetoes a non-raising increment; XX on a missing member too.
    assert_eq!(
        call(&s, "zadd", &[b"k", b"GT", b"INCR", b"-1", b"a"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"XX", b"INCR", b"1", b"zz"]),
        b"$-1\r\n".to_vec()
    );
    // -inf + inf overflows into NaN.
    zadd3(&s, b"n", b"m", b"-inf", b"x", b"1");
    assert_eq!(
        call(&s, "zadd", &[b"n", b"INCR", b"inf", b"m"]),
        b"-ERR resulting score is not a number\r\n".to_vec()
    );
}

#[test]
fn zincrby_adds_updates_and_rejects_nan() {
    let (_g, s) = shared_for("127.0.0.1:40506");
    assert_eq!(
        call(&s, "zincrby", &[b"k", b"1.5", b"a"]),
        b"$3\r\n1.5\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zincrby", &[b"k", b"-0.5", b"a"]),
        b"$1\r\n1\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 1);
    assert_eq!(
        call(&s, "zincrby", &[b"k", b"xx", b"a"]),
        b"-ERR value is not a valid float\r\n".to_vec()
    );
    call(&s, "zadd", &[b"n", b"-inf", b"m"]);
    assert_eq!(
        call(&s, "zincrby", &[b"n", b"inf", b"m"]),
        b"-ERR resulting score is not a number\r\n".to_vec()
    );
}

#[test]
fn zscore_and_zmscore_formats() {
    let (_g, s) = shared_for("127.0.0.1:40507");
    call(&s, "zadd", &[b"k", b"1.5", b"a", b"2", b"b", b"inf", b"c"]);
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$3\r\n1.5\r\n".to_vec());
    assert_eq!(call(&s, "zscore", &[b"k", b"c"]), b"$3\r\ninf\r\n".to_vec());
    assert_eq!(call(&s, "zscore", &[b"k", b"zz"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "zscore", &[b"none", b"a"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&s, "zmscore", &[b"k", b"b", b"zz", b"a"]),
        b"*3\r\n$1\r\n2\r\n$-1\r\n$3\r\n1.5\r\n".to_vec()
    );
    // Missing key: one null per member.
    assert_eq!(
        call(&s, "zmscore", &[b"none", b"a", b"b"]),
        b"*2\r\n$-1\r\n$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zmscore", &[b"k"]),
        b"-ERR wrong number of arguments for 'zmscore' command\r\n".to_vec()
    );
}

#[test]
fn zcount_score_bounds() {
    let (_g, s) = shared_for("127.0.0.1:40508");
    call(
        &s,
        "zadd",
        &[b"k", b"1", b"a", b"2", b"b", b"3", b"c", b"4", b"d"],
    );
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"-inf", b"+inf"])), 4);
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"2", b"3"])), 2);
    // Exclusive min `(`1 drops a, exclusive-max-ish combo keeps b..c.
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"(1", b"3"])), 2);
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"(2", b"(4"])), 1);
    assert_eq!(int_of(&call(&s, "zcount", &[b"k", b"4", b"1"])), 0);
    assert_eq!(int_of(&call(&s, "zcount", &[b"none", b"1", b"2"])), 0);
    assert_eq!(
        call(&s, "zcount", &[b"k", b"zz", b"2"]),
        b"-ERR min or max not valid float\r\n".to_vec()
    );
}

#[test]
fn zrank_zrevrank_withscore() {
    let (_g, s) = shared_for("127.0.0.1:40509");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2", b"b", b"3", b"c"]);
    assert_eq!(int_of(&call(&s, "zrank", &[b"k", b"a"])), 0);
    assert_eq!(int_of(&call(&s, "zrank", &[b"k", b"c"])), 2);
    assert_eq!(int_of(&call(&s, "zrevrank", &[b"k", b"c"])), 0);
    assert_eq!(int_of(&call(&s, "zrevrank", &[b"k", b"a"])), 2);
    assert_eq!(
        call(&s, "zrank", &[b"k", b"a", b"WITHSCORE"]),
        b"*2\r\n:0\r\n$1\r\n1\r\n".to_vec()
    );
    assert_eq!(call(&s, "zrank", &[b"k", b"zz"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&s, "zrank", &[b"k", b"zz", b"WITHSCORE"]),
        b"*-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrank", &[b"k", b"a", b"NOPE"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

#[test]
fn zrem_removes_and_deletes_empty_zset() {
    let (_g, s) = shared_for("127.0.0.1:40510");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2", b"b", b"3", b"c"]);
    assert_eq!(int_of(&call(&s, "zrem", &[b"k", b"zz"])), 0);
    assert_eq!(int_of(&call(&s, "zrem", &[b"k", b"b", b"zz", b"a"])), 2);
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 1);
    // Duplicate arguments count once; the last member deletes the key.
    assert_eq!(int_of(&call(&s, "zrem", &[b"k", b"c", b"c"])), 1);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    assert_eq!(int_of(&call(&s, "zrem", &[b"none", b"a"])), 0);
}

#[test]
fn zpopmin_zpopmax_flat_replies() {
    let (_g, s) = shared_for("127.0.0.1:40511");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2", b"b", b"3", b"c"]);
    assert_eq!(
        call(&s, "zpopmin", &[b"k"]),
        b"*2\r\n$1\r\na\r\n$1\r\n1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zpopmax", &[b"k", b"2"]),
        b"*4\r\n$1\r\nc\r\n$1\r\n3\r\n$1\r\nb\r\n$1\r\n2\r\n".to_vec()
    );
    assert_eq!(int_of(&call(&s, "zcard", &[b"k"])), 0);
    assert_eq!(call(&s, "zpopmin", &[b"k"]), b"*0\r\n".to_vec());
    assert_eq!(call(&s, "zpopmin", &[b"none"]), b"*0\r\n".to_vec());
    assert_eq!(call(&s, "zpopmin", &[b"none", b"3"]), b"*0\r\n".to_vec());
    call(&s, "zadd", &[b"j", b"1", b"x"]);
    assert_eq!(call(&s, "zpopmin", &[b"j", b"0"]), b"*0\r\n".to_vec());
    assert_eq!(
        call(&s, "zpopmin", &[b"j", b"-1"]),
        b"-ERR value is out of range, must be positive\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zpopmax", &[b"j", b"zz"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    let bulks = bulks_of(&call(&s, "zpopmin", &[b"j", b"9"]));
    assert_eq!(bulks, vec![b"x".to_vec(), b"1".to_vec()]);
}

#[test]
fn zrandmember_shapes() {
    let (_g, s) = shared_for("127.0.0.1:40512");
    call(&s, "zadd", &[b"k", b"1", b"a", b"2", b"b", b"3", b"c"]);
    // No count: one random member.
    let pick = bulk_of(&call(&s, "zrandmember", &[b"k"]));
    assert!(matches!(pick.as_slice(), b"a" | b"b" | b"c"));
    assert_eq!(call(&s, "zrandmember", &[b"none"]), b"$-1\r\n".to_vec());
    // Positive count: distinct members, at most the set size.
    let picks = bulks_of(&call(&s, "zrandmember", &[b"k", b"2"]));
    assert_eq!(picks.len(), 2);
    assert_ne!(picks[0], picks[1]);
    assert!(picks
        .iter()
        .all(|m| matches!(m.as_slice(), b"a" | b"b" | b"c")));
    assert_eq!(bulks_of(&call(&s, "zrandmember", &[b"k", b"10"])).len(), 3);
    // Negative count: repeating draws, WITHVALUES interleaves scores.
    let rep = bulks_of(&call(&s, "zrandmember", &[b"k", b"-6"]));
    assert_eq!(rep.len(), 6);
    let withv = bulks_of(&call(&s, "zrandmember", &[b"k", b"2", b"WITHVALUES"]));
    assert_eq!(withv.len(), 4);
    assert!(withv
        .iter()
        .any(|v| matches!(v.as_slice(), b"1" | b"2" | b"3")));
    assert_eq!(
        call(&s, "zrandmember", &[b"k", b"2", b"NOPE"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zrandmember", &[b"none", b"3"]),
        b"*0\r\n".to_vec()
    );
}
