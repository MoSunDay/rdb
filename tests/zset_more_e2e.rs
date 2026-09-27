//! E2e for the second zset batch: ZCOUNT/ZMSCORE/ZREVRANK/ZRANDMEMBER,
//! ZREM/ZPOPMAX/ZREMRANGEBYSCORE and the lex + rev score windows
//! (ZRANGEBYLEX/ZREVRANGEBYLEX/ZLEXCOUNT/ZREVRANGEBYSCORE).

mod common;

use common::lite::{call, shared_at};

const WRONGTYPE: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
const NOT_FLOAT: &[u8] = b"-ERR min or max not valid float\r\n";
const NOT_LEX: &[u8] = b"-ERR min or max not valid string range item\r\n";
const NOT_INT: &[u8] = b"-ERR value is not an integer or out of range\r\n";
const POP_RANGE: &[u8] = b"-ERR value is out of range, must be positive\r\n";
const SYNTAX: &[u8] = b"-ERR syntax error\r\n";

/// Flat array-of-bulks frame from member (or member,score) strings.
fn arr(items: &[&str]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", items.len()).into_bytes();
    for i in items {
        out.extend_from_slice(format!("${}\r\n{}\r\n", i.len(), i).as_bytes());
    }
    out
}

/// Bulk payloads of a flat array (or single-bulk) reply -- for the
/// order-unstable ZRANDMEMBER shapes.
fn bulks(reply: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(reply).into_owned();
    let mut lines = text.split("\r\n").filter(|l| !l.is_empty());
    let mut out = Vec::new();
    while let Some(head) = lines.next() {
        if let Some(len) = head.strip_prefix('$') {
            if let Some(body) = lines.next() {
                let len: usize = len.parse().unwrap_or(0);
                out.push(body[..len.min(body.len())].to_string());
            }
        }
    }
    out
}

#[test]
fn zcount_windows_wrongtype_and_arity() {
    let (shared, _path) = shared_at("45230");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &["zc", "1", "a", "2", "b", "3", "c", "3", "d", "5", "e"],
        b":5\r\n",
    );
    e("zcount", &["zc", "1", "5"], b":5\r\n");
    e("zcount", &["zc", "(1", "5"], b":4\r\n");
    e("zcount", &["zc", "(1", "(5"], b":3\r\n");
    e("zcount", &["zc", "3", "3"], b":2\r\n");
    e("zcount", &["zc", "-inf", "+inf"], b":5\r\n");
    e("zcount", &["zc", "-inf", "(5"], b":4\r\n");
    e("zcount", &["zc", "10", "1"], b":0\r\n");
    e("zcount", &["nokey", "1", "2"], b":0\r\n");
    e("zcount", &["zc", "1", "abc"], NOT_FLOAT);
    e("set", &["str", "v"], b"+OK\r\n");
    e("zcount", &["str", "1", "2"], WRONGTYPE);
}

#[test]
fn zmscore_nil_gaps_and_missing_key() {
    let (shared, _path) = shared_at("45231");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e("zadd", &["zm", "1.5", "one", "2", "two"], b":2\r\n");
    e(
        "zmscore",
        &["zm", "one", "two", "nope"],
        b"*3\r\n$3\r\n1.5\r\n$1\r\n2\r\n$-1\r\n",
    );
    e("zmscore", &["zm", "nope"], b"*1\r\n$-1\r\n");
    e("zmscore", &["nokey", "a", "b"], b"*2\r\n$-1\r\n$-1\r\n");
}

#[test]
fn zrevrank_ranks_withscore_and_missing() {
    let (shared, _path) = shared_at("45232");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &["zr", "10", "a", "20", "b", "30", "c", "40", "d"],
        b":4\r\n",
    );
    e("zrevrank", &["zr", "d"], b":0\r\n");
    e("zrevrank", &["zr", "b"], b":2\r\n");
    e("zrevrank", &["zr", "nope"], b"$-1\r\n");
    e("zrevrank", &["nokey", "a"], b"$-1\r\n");
    e(
        "zrevrank",
        &["zr", "b", "WITHSCORE"],
        b"*2\r\n:2\r\n$2\r\n20\r\n",
    );
    e("zrevrank", &["zr", "nope", "WITHSCORE"], b"*-1\r\n");
    e("zrevrank", &["zr", "b", "NOPE"], SYNTAX);
}

#[test]
fn zrandmember_count_variants() {
    let (shared, _path) = shared_at("45233");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    let scores = [("a", "1"), ("b", "2"), ("c", "3")];
    e("zadd", &["zq", "1", "a", "2", "b", "3", "c"], b":3\r\n");
    // No count: ONE random member -- shape only, the pick is not stable.
    let one = bulks(&call(&shared, "zrandmember", &[b"zq"]));
    assert_eq!(one.len(), 1);
    assert!(scores.iter().any(|s| s.0 == one[0]));
    e("zrandmember", &["zq", "0"], b"*0\r\n");
    e("zrandmember", &["nokey"], b"$-1\r\n");
    e("zrandmember", &["nokey", "3"], b"*0\r\n");
    // +count: DISTINCT picks capped at the cardinality (header pins it).
    let three = bulks(&call(&shared, "zrandmember", &[b"zq", b"3"]));
    let mut sorted = three.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["a".to_string(), "b".into(), "c".into()]);
    assert!(call(&shared, "zrandmember", &[b"zq", b"10"]).starts_with(b"*3\r\n"));
    // -count: |count| draws WITH replacement.
    let rep = bulks(&call(&shared, "zrandmember", &[b"zq", b"-7"]));
    assert_eq!(rep.len(), 7);
    assert!(rep.iter().all(|m| scores.iter().any(|s| s.0 == *m)));
    // WITHVALUES: member,score pairs, members still distinct under +count.
    let pairs = bulks(&call(&shared, "zrandmember", &[b"zq", b"2", b"WITHVALUES"]));
    assert_eq!(pairs.len(), 4);
    for pair in pairs.chunks(2) {
        assert!(
            scores.contains(&(pair[0].as_str(), pair[1].as_str())),
            "{pairs:?}"
        );
    }
    let members: Vec<&str> = pairs.chunks(2).map(|p| p[0].as_str()).collect();
    let mut distinct = members.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(members.len(), distinct.len());
    e("zrandmember", &["zq", "x"], NOT_INT);
    e("zrandmember", &["zq", "1", "NOPE"], SYNTAX);
}

#[test]
fn zrem_multi_drains_and_dedupes() {
    let (shared, _path) = shared_at("45234");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e("zadd", &["zm2", "1", "a", "2", "b", "3", "c"], b":3\r\n");
    e("zrem", &["zm2", "a", "nope", "b"], b":2\r\n");
    e("zcard", &["zm2"], b":1\r\n");
    e("zrem", &["zm2", "a"], b":0\r\n");
    e("zrem", &["zm2", "c"], b":1\r\n");
    e("exists", &["zm2"], b":0\r\n");
    e("zrem", &["nokey", "a"], b":0\r\n");
    e("zadd", &["zd", "1", "a", "2", "b"], b":2\r\n");
    e("zrem", &["zd", "a", "a", "a"], b":1\r\n");
}

#[test]
fn zpopmax_default_count_and_pairs() {
    let (shared, _path) = shared_at("45235");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &["zp", "1", "low", "2.5", "mid", "3", "high"],
        b":3\r\n",
    );
    e("zpopmax", &["zp"], b"*2\r\n$4\r\nhigh\r\n$1\r\n3\r\n");
    e("zpopmax", &["nokey"], b"*0\r\n");
    e("zadd", &["zp2", "1", "a", "2", "b", "3", "c"], b":3\r\n");
    e(
        "zpopmax",
        &["zp2", "2"],
        b"*4\r\n$1\r\nc\r\n$1\r\n3\r\n$1\r\nb\r\n$1\r\n2\r\n",
    );
    e("zpopmax", &["zp2", "99"], b"*2\r\n$1\r\na\r\n$1\r\n1\r\n");
    e("exists", &["zp2"], b":0\r\n");
    e("zpopmax", &["zp3", "0"], b"*0\r\n");
    e("zpopmax", &["zp3", "-1"], POP_RANGE);
    e("zpopmax", &["zp3", "x"], NOT_INT);
}

#[test]
fn zrevrangebyscore_bounds_withscores_limit() {
    let (shared, _path) = shared_at("45236");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &["zs", "1", "a", "2", "b", "3", "c", "4", "d", "5", "e"],
        b":5\r\n",
    );
    e(
        "zrevrangebyscore",
        &["zs", "5", "1"],
        &arr(&["e", "d", "c", "b", "a"]),
    );
    e(
        "zrevrangebyscore",
        &["zs", "(5", "1"],
        &arr(&["d", "c", "b", "a"]),
    );
    e(
        "zrevrangebyscore",
        &["zs", "5", "(1"],
        &arr(&["e", "d", "c", "b"]),
    );
    e("zrevrangebyscore", &["zs", "3", "3"], &arr(&["c"]));
    e(
        "zrevrangebyscore",
        &["zs", "+inf", "-inf", "WITHSCORES"],
        &arr(&["e", "5", "d", "4", "c", "3", "b", "2", "a", "1"]),
    );
    // LIMIT offset counts in REPLY (descending) order.
    e(
        "zrevrangebyscore",
        &["zs", "+inf", "-inf", "LIMIT", "1", "2"],
        &arr(&["d", "c"]),
    );
    e("zrevrangebyscore", &["nokey", "5", "1"], b"*0\r\n");
    e("zrevrangebyscore", &["zs", "1", "5"], b"*0\r\n");
    e("zrevrangebyscore", &["zs", "abc", "1"], NOT_FLOAT);
}

#[test]
fn zrangebylex_and_zlexcount_semantics() {
    let (shared, _path) = shared_at("45237");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &[
            "zl", "0", "a", "0", "b", "0", "c", "0", "d", "0", "e", "0", "f",
        ],
        b":6\r\n",
    );
    e(
        "zrangebylex",
        &["zl", "-", "+"],
        &arr(&["a", "b", "c", "d", "e", "f"]),
    );
    e("zrangebylex", &["zl", "[b", "[d"], &arr(&["b", "c", "d"]));
    e("zrangebylex", &["zl", "(b", "(e"], &arr(&["c", "d"]));
    e("zrangebylex", &["zl", "-", "[c"], &arr(&["a", "b", "c"]));
    e("zrangebylex", &["zl", "[d", "+"], &arr(&["d", "e", "f"]));
    e(
        "zrangebylex",
        &["zl", "-", "+", "LIMIT", "1", "2"],
        &arr(&["b", "c"]),
    );
    e("zrangebylex", &["nokey", "-", "+"], b"*0\r\n");
    e("zrangebylex", &["zl", "b", "d"], NOT_LEX);
    e("zlexcount", &["zl", "-", "+"], b":6\r\n");
    // [b is inclusive of b, (e exclusive of e: b, c, d.
    e("zlexcount", &["zl", "[b", "(e"], b":3\r\n");
    e("zlexcount", &["nokey", "-", "+"], b":0\r\n");
    e("zlexcount", &["zl", "b", "+"], NOT_LEX);
}

#[test]
fn zrevrangebylex_success_paths() {
    let (shared, _path) = shared_at("45238");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &[
            "zvl", "0", "apple", "0", "banana", "0", "cherry", "0", "date", "0", "fig",
        ],
        b":5\r\n",
    );
    e(
        "zrevrangebylex",
        &["zvl", "+", "-"],
        &arr(&["fig", "date", "cherry", "banana", "apple"]),
    );
    e(
        "zrevrangebylex",
        &["zvl", "[cherry", "-"],
        &arr(&["cherry", "banana", "apple"]),
    );
    e(
        "zrevrangebylex",
        &["zvl", "+", "(cherry"],
        &arr(&["fig", "date"]),
    );
    e(
        "zrevrangebylex",
        &["zvl", "(fig", "[banana"],
        &arr(&["date", "cherry", "banana"]),
    );
    e(
        "zrevrangebylex",
        &["zvl", "[cherry", "[cherry"],
        &arr(&["cherry"]),
    );
    e(
        "zrevrangebylex",
        &["zvl", "+", "-", "LIMIT", "1", "2"],
        &arr(&["date", "cherry"]),
    );
    // [a sits below every member ("apple" sorts after "a"): empty.
    e("zrevrangebylex", &["zvl", "[a", "-"], b"*0\r\n");
    e("zrevrangebylex", &["nokey", "+", "-"], b"*0\r\n");
    e("zrevrangebylex", &["zvl", "+", "apple"], NOT_LEX);
    e("set", &["str", "v"], b"+OK\r\n");
    e("zrevrangebylex", &["str", "+", "-"], WRONGTYPE);
}

#[test]
fn zremrangebyscore_returns_removed_count() {
    let (shared, _path) = shared_at("45239");
    let e = |name: &str, args: &[&str], want: &[u8]| {
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want, "'{name}' reply mismatch");
    };
    e(
        "zadd",
        &["zrs", "1", "a", "2", "b", "3", "c", "4", "d"],
        b":4\r\n",
    );
    e("zremrangebyscore", &["zrs", "(2", "3"], b":1\r\n");
    e(
        "zrevrangebyscore",
        &["zrs", "+inf", "-inf"],
        &arr(&["d", "b", "a"]),
    );
    e("zremrangebyscore", &["zrs", "-inf", "+inf"], b":3\r\n");
    e("exists", &["zrs"], b":0\r\n");
    e("zremrangebyscore", &["nokey", "1", "2"], b":0\r\n");
    e("zremrangebyscore", &["zrs", "abc", "2"], NOT_FLOAT);
}

#[test]
fn arity_errors_are_the_exact_redis_text() {
    let (shared, _path) = shared_at("45240");
    let cases: &[(&str, &[&str])] = &[
        ("zcount", &["k", "1"]),
        ("zmscore", &["k"]),
        ("zrevrank", &["k"]),
        ("zrandmember", &["k", "1", "WITHVALUES", "x"]),
        ("zrem", &["k"]),
        ("zpopmax", &[]),
        ("zrevrangebyscore", &["k", "5"]),
        ("zlexcount", &["k", "-"]),
        ("zrevrangebylex", &["k", "+"]),
        ("zremrangebyscore", &["k", "1"]),
    ];
    for (name, args) in cases {
        let want = format!("-ERR wrong number of arguments for '{name}' command\r\n");
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(call(&shared, name, &argv), want.as_bytes(), "{name}");
    }
}
