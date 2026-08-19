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
    // GT only raises existing scores (Redis: missing members are still
    // ADDED), CH counts updates as well as additions.
    assert_eq!(
        int_of(&call(
            &s,
            "zadd",
            &[b"k", b"GT", b"CH", b"3", b"a", b"1", b"zz"]
        )),
        2
    );
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$1\r\n3\r\n".to_vec());
    assert_eq!(call(&s, "zscore", &[b"k", b"zz"]), b"$1\r\n1\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"gt", b"2", b"a"])), 0);
    // LT only lowers existing scores, but still adds missing members.
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"k", b"LT", b"CH", b"1", b"a"])),
        1
    );
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"lt", b"9", b"a"])), 0);
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"k", b"LT", b"CH", b"0.5", b"yy"])),
        1
    );
    assert_eq!(
        call(&s, "zscore", &[b"k", b"yy"]),
        b"$3\r\n0.5\r\n".to_vec()
    );
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

/// A NEW member created by ZINCRBY -0 keeps the sign: the naive
/// `0.0 + (-0.0)` collapses to +0.0 and loses the "-0" both the reply
/// and a later ZSCORE must show (regression guard for the fix).
#[test]
fn zincrby_negative_zero_on_new_member_keeps_sign() {
    let (_g, s) = shared_for("127.0.0.1:40570");
    assert_eq!(
        call(&s, "zincrby", &[b"k", b"-0", b"a"]),
        b"$2\r\n-0\r\n".to_vec()
    );
    // The STORED score keeps the sign, not just the reply.
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$2\r\n-0\r\n".to_vec());
    // Non-zero negatives on new members keep working.
    assert_eq!(
        call(&s, "zincrby", &[b"k", b"-2.5", b"b"]),
        b"$4\r\n-2.5\r\n".to_vec()
    );
    // An EXISTING member still goes through the add path.
    assert_eq!(
        call(&s, "zincrby", &[b"k", b"2.5", b"b"]),
        b"$1\r\n0\r\n".to_vec()
    );
}

/// ZADD ... INCR -0 on a missing member: same trap as ZINCRBY -- the
/// delta must land verbatim, reply AND stored score both "-0".
#[test]
fn zadd_incr_negative_zero_keeps_sign() {
    let (_g, s) = shared_for("127.0.0.1:40571");
    assert_eq!(
        call(&s, "zadd", &[b"k", b"INCR", b"-0", b"a"]),
        b"$2\r\n-0\r\n".to_vec()
    );
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$2\r\n-0\r\n".to_vec());
    // Incrementing that member onwards still adds normally.
    assert_eq!(
        call(&s, "zadd", &[b"k", b"INCR", b"1", b"a"]),
        b"$1\r\n1\r\n".to_vec()
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

#[test]
fn zscan_pages_over_an_empty_member() {
    let (_g, s) = shared_for("127.0.0.1:40514");
    let argv: Vec<Vec<u8>> = vec![
        b"k".to_vec(),
        b"1".to_vec(),
        Vec::new(),
        b"2".to_vec(),
        b"a".to_vec(),
        b"3".to_vec(),
        b"b".to_vec(),
    ];
    call(
        &s,
        "zadd",
        &argv.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
    );
    // COUNT 1 puts the empty member on a page boundary: the hex cursor of
    // "" is "" itself, which must resume STRICTLY AFTER "" — a restart
    // there would loop forever, and the old empty-bytes sentinel misread
    // it as "done", silently dropping the remaining members.
    let mut cursor = b"0".to_vec();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;
    loop {
        let page = test_reader::parse(&call(&s, "zscan", &[b"k", &cursor, b"COUNT", b"1"]));
        cursor = test_reader::bulk(&page[0]);
        if page.len() > 1 {
            seen.push(test_reader::bulk(&page[1]));
        }
        pages += 1;
        if cursor == b"0" {
            break;
        }
        assert!(pages < 32, "cursor loop did not terminate");
    }
    seen.sort();
    assert_eq!(seen, vec![b"".to_vec(), b"a".to_vec(), b"b".to_vec()]);
    // WITHVALUES keeps the flat [cursor, member, score, ...] shape.
    let reply = call(&s, "zscan", &[b"k", b"0", b"COUNT", b"1", b"WITHSCORES"]);
    let flat = bulks_of(&reply);
    assert_eq!(flat[0], b"".to_vec(), "cursor of the empty member");
    assert_eq!(flat[1], b"".to_vec(), "the empty member itself");
    assert_eq!(flat[2], b"1".to_vec(), "its score");
}

/// CH counts new members and genuine score changes, but NOT a
/// same-score re-add of an existing member (Redis reports those as
/// unchanged); without CH the reply is new members only.
#[test]
fn zadd_ch_ignores_same_score_readds() {
    let (_g, s) = shared_for("127.0.0.1:40515");
    zadd3(&s, b"k", b"a", b"1", b"b", b"2");
    // Re-adding existing members with identical scores: CH reports 0.
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"k", b"CH", b"1", b"a", b"2", b"b"])),
        0
    );
    // One changed score plus one new member: CH reports 2.
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"k", b"CH", b"7", b"a", b"9", b"c"])),
        2
    );
    // A same-score re-add mixed with a genuine change counts just the one.
    assert_eq!(
        int_of(&call(&s, "zadd", &[b"k", b"CH", b"7", b"a", b"4", b"b"])),
        1
    );
    // Without CH only NEW members are counted: two updates, one add -> 1.
    assert_eq!(
        int_of(&call(
            &s,
            "zadd",
            &[b"k", b"8", b"a", b"5", b"b", b"6", b"c3"]
        )),
        1
    );
    assert_eq!(call(&s, "zscore", &[b"k", b"a"]), b"$1\r\n8\r\n".to_vec());
    assert_eq!(call(&s, "zscore", &[b"k", b"b"]), b"$1\r\n5\r\n".to_vec());
    assert_eq!(call(&s, "zscore", &[b"k", b"c3"]), b"$1\r\n6\r\n".to_vec());
    // An unchanged member also stays uncounted without CH.
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"8", b"a"])), 0);
}

/// Regression (Redis semantics): GT/LT only gate UPDATES of existing
/// members -- NEW members are always added, plain and INCR alike.
#[test]
fn zadd_gt_lt_add_new_members() {
    let (_g, s) = shared_for("127.0.0.1:40516");
    // Plain GT/LT add missing members verbatim and count them.
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"GT", b"5", b"m"])), 1);
    assert_eq!(call(&s, "zscore", &[b"k", b"m"]), b"$1\r\n5\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"LT", b"-3", b"n"])), 1);
    assert_eq!(call(&s, "zscore", &[b"k", b"n"]), b"$2\r\n-3\r\n".to_vec());
    // GT INCR on a missing member adds it and replies the new score.
    assert_eq!(
        call(&s, "zadd", &[b"k", b"GT", b"INCR", b"2.5", b"fresh"]),
        b"$3\r\n2.5\r\n".to_vec()
    );
    // GT INCR on an existing member only applies when it raises.
    assert_eq!(
        call(&s, "zadd", &[b"k", b"GT", b"INCR", b"-1", b"m"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "zadd", &[b"k", b"GT", b"INCR", b"1", b"m"]),
        b"$1\r\n6\r\n".to_vec()
    );
    // Plain GT on an existing, not-raising score stays uncounted.
    assert_eq!(int_of(&call(&s, "zadd", &[b"k", b"GT", b"1", b"m"])), 0);
    assert_eq!(call(&s, "zscore", &[b"k", b"m"]), b"$1\r\n6\r\n".to_vec());
}
