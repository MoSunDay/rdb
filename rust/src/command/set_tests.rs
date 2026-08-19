//! Handler-level tests for the set commands (SADD..SSCAN, set algebra):
//! crate-wide store lock, fresh `Shared` per test, dispatch through the
//! command registry so the registered names are exercised too. Multi-key
//! commands use `{g}`-tagged keys so every operand hashes to one slot
//! (the physical prefix is fixed at `PREFIX` regardless); `{u}`-tagged
//! keys are the cross-slot outgroup.

use crate::command::test_ctx;
use crate::command::Handler;
use crate::resp::codec::test_reader::{self, Frame};
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

fn members(shared: &Shared, key: &[u8]) -> Vec<Vec<u8>> {
    let mut all = bulks_of(&call(shared, "smembers", &[key]));
    all.sort();
    all
}

fn set_raw(shared: &Shared, key: &[u8], val: &[u8]) {
    crate::store::set(&shared.store, PREFIX, key, val).expect("raw set");
}

#[test]
fn sadd_srem_counts_and_key_lifetime() {
    let (_g, s) = shared_for("127.0.0.1:40311");
    assert_eq!(int_of(&call(&s, "sadd", &[b"k", b"a", b"b", b"c"])), 3);
    // Duplicates only count newly added members.
    assert_eq!(int_of(&call(&s, "sadd", &[b"k", b"b", b"d"])), 1);
    assert_eq!(int_of(&call(&s, "scard", &[b"k"])), 4);
    assert_eq!(int_of(&call(&s, "srem", &[b"k", b"zz", b"a"])), 1);
    // Removing the last members deletes the key entirely.
    assert_eq!(int_of(&call(&s, "srem", &[b"k", b"b", b"c", b"d"])), 3);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    assert_eq!(int_of(&call(&s, "scard", &[b"k"])), 0);
    assert_eq!(call(&s, "smembers", &[b"k"]), b"*0\r\n".to_vec());
}

#[test]
fn sismember_and_smismember() {
    let (_g, s) = shared_for("127.0.0.1:40312");
    call(&s, "sadd", &[b"k", b"a"]);
    assert_eq!(int_of(&call(&s, "sismember", &[b"k", b"a"])), 1);
    assert_eq!(int_of(&call(&s, "sismember", &[b"k", b"zz"])), 0);
    assert_eq!(int_of(&call(&s, "sismember", &[b"none", b"a"])), 0);
    assert_eq!(
        call(&s, "smismember", &[b"k", b"zz", b"a"]),
        b"*2\r\n:0\r\n:1\r\n".to_vec()
    );
}

#[test]
fn spop_shrinks_and_deletes_key() {
    let (_g, s) = shared_for("127.0.0.1:40313");
    call(&s, "sadd", &[b"k", b"a", b"b"]);
    // Single pop: one member back; the set shrinks to 1.
    let popped = bulk_of(&call(&s, "spop", &[b"k"]));
    assert!(popped == b"a" || popped == b"b");
    assert_eq!(int_of(&call(&s, "scard", &[b"k"])), 1);
    // Count pop returns the rest and removes the key.
    let rest = bulks_of(&call(&s, "spop", &[b"k", b"10"]));
    assert_eq!(rest.len(), 1);
    assert_ne!(rest[0], popped);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    // Missing key pops nothing.
    assert_eq!(call(&s, "spop", &[b"none"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "spop", &[b"none", b"3"]), b"*0\r\n".to_vec());
    // Non-positive / non-integer counts error.
    call(&s, "sadd", &[b"j", b"x"]);
    assert_eq!(
        call(&s, "spop", &[b"j", b"0"]),
        b"-ERR value is out of range, must be positive\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "spop", &[b"j", b"zz"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn srandmember_negative_repeats() {
    let (_g, s) = shared_for("127.0.0.1:40314");
    call(&s, "sadd", &[b"k", b"a", b"b"]);
    let one = bulk_of(&call(&s, "srandmember", &[b"k"]));
    assert!(one == b"a" || one == b"b");
    // Positive count caps at cardinality and never repeats.
    let two = bulks_of(&call(&s, "srandmember", &[b"k", b"5"]));
    assert_eq!(two.len(), 2);
    // Negative count may repeat and ignores cardinality.
    let rep = bulks_of(&call(&s, "srandmember", &[b"k", b"-6"]));
    assert_eq!(rep.len(), 6);
    assert_eq!(call(&s, "srandmember", &[b"none"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&s, "srandmember", &[b"none", b"3"]),
        b"*0\r\n".to_vec()
    );
}

#[test]
fn sscan_pages_and_matches() {
    let (_g, s) = shared_for("127.0.0.1:40315");
    call(&s, "sadd", &[b"k", b"m1", b"m2", b"m3", b"m4", b"m5"]);
    // COUNT 2 pages through all five members exactly once.
    let mut cursor = b"0".to_vec();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    loop {
        let page = test_reader::parse(&call(&s, "sscan", &[b"k", &cursor, b"COUNT", b"2"]));
        cursor = test_reader::bulk(&page[0]);
        let Frame::Array(items) = &page[1] else {
            panic!("items array");
        };
        seen.extend(items.iter().map(test_reader::bulk));
        if cursor == b"0" {
            break;
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![
            b"m1".to_vec(),
            b"m2".to_vec(),
            b"m3".to_vec(),
            b"m4".to_vec(),
            b"m5".to_vec()
        ]
    );
    // MATCH filters members.
    let page = test_reader::parse(&call(&s, "sscan", &[b"k", b"0", b"MATCH", b"m[13]"]));
    let Frame::Array(items) = &page[1] else {
        panic!("items array");
    };
    let got: Vec<Vec<u8>> = items.iter().map(test_reader::bulk).collect();
    assert_eq!(got, vec![b"m1".to_vec(), b"m3".to_vec()]);
    assert_eq!(
        call(&s, "sscan", &[b"k", b"zz"]),
        b"-ERR invalid cursor\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "sscan", &[b"k", b"0", b"NOPE"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

#[test]
fn sscan_pages_over_an_empty_member() {
    let (_g, s) = shared_for("127.0.0.1:40513");
    call(&s, "sadd", &[b"k", b"", b"a", b"b"]);
    // COUNT 1 puts the empty member on a page boundary: the hex cursor of
    // "" is "" itself, which must resume STRICTLY AFTER "" — a restart
    // there would loop forever, and the old empty-bytes sentinel misread
    // it as "done", silently dropping the remaining members.
    let mut cursor = b"0".to_vec();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;
    loop {
        let page = test_reader::parse(&call(&s, "sscan", &[b"k", &cursor, b"COUNT", b"1"]));
        cursor = test_reader::bulk(&page[0]);
        let Frame::Array(items) = &page[1] else {
            panic!("items array");
        };
        seen.extend(items.iter().map(test_reader::bulk));
        pages += 1;
        if cursor == b"0" {
            break;
        }
        assert!(pages < 32, "cursor loop did not terminate");
    }
    seen.sort();
    assert_eq!(seen, vec![Vec::new(), b"a".to_vec(), b"b".to_vec()]);
    // Sanity: unbounded page returns everything with the reset cursor.
    let page = test_reader::parse(&call(&s, "sscan", &[b"k", b"0"]));
    let Frame::Array(items) = &page[1] else {
        panic!("items array");
    };
    let mut all: Vec<Vec<u8>> = items.iter().map(test_reader::bulk).collect();
    all.sort();
    assert_eq!(all, vec![Vec::new(), b"a".to_vec(), b"b".to_vec()]);
    assert_eq!(test_reader::bulk(&page[0]), b"0".to_vec());
}

#[test]
fn sscan_negated_class_match_over_empty_member() {
    let (_g, s) = shared_for("127.0.0.1:40770");
    call(&s, "sadd", &[b"k", b"", b"a", b"b"]);
    // MATCH [^a]* runs the empty member through glob_match with a
    // negated class; a class consumes a byte, so "" must simply not
    // match (the old matcher panicked slicing past the empty member).
    let page = test_reader::parse(&call(&s, "sscan", &[b"k", b"0", b"MATCH", b"[^a]*"]));
    let Frame::Array(items) = &page[1] else {
        panic!("items array");
    };
    // "b" matches ([^a] eats it, '*' eats nothing); "" cannot (the
    // class needs a byte) and "a" is excluded by the negation.
    let mut got: Vec<Vec<u8>> = items.iter().map(test_reader::bulk).collect();
    got.sort();
    assert_eq!(got, vec![b"b".to_vec()]);
    assert_eq!(test_reader::bulk(&page[0]), b"0".to_vec());
}

#[test]
fn smove_both_directions_and_crossslot() {
    let (_g, s) = shared_for("127.0.0.1:40316");
    let (src, dst, same) = (
        b"{g}src".as_slice(),
        b"{g}dst".as_slice(),
        b"{g}same".as_slice(),
    );
    call(&s, "sadd", &[src, b"a", b"b"]);
    call(&s, "sadd", &[dst, b"c"]);
    assert_eq!(int_of(&call(&s, "smove", &[src, dst, b"a"])), 1);
    assert_eq!(members(&s, src), vec![b"b".to_vec()]);
    assert_eq!(members(&s, dst), vec![b"a".to_vec(), b"c".to_vec()]);
    // Member no longer present -> 0, nothing changes.
    assert_eq!(int_of(&call(&s, "smove", &[src, dst, b"a"])), 0);
    // Moving the last member away deletes the source key.
    assert_eq!(int_of(&call(&s, "smove", &[src, dst, b"b"])), 1);
    assert_eq!(int_of(&call(&s, "exists", &[src])), 0);
    // Missing source -> 0.
    assert_eq!(int_of(&call(&s, "smove", &[b"{g}none", dst, b"x"])), 0);
    // Same key is a no-op 1 when the member exists.
    call(&s, "sadd", &[same, b"a"]);
    assert_eq!(int_of(&call(&s, "smove", &[same, same, b"a"])), 1);
    // Cross-slot keys are rejected before any mutation.
    let before = members(&s, dst);
    assert_eq!(
        call(&s, "smove", &[dst, b"{u}other", b"c"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    assert_eq!(members(&s, dst), before);
}

#[test]
fn set_algebra_read_variants() {
    let (_g, s) = shared_for("127.0.0.1:40317");
    let (a, b, missing) = (
        b"{g}a".as_slice(),
        b"{g}b".as_slice(),
        b"{g}missing".as_slice(),
    );
    call(&s, "sadd", &[a, b"x", b"y"]);
    call(&s, "sadd", &[b, b"y", b"z"]);
    // Missing operands count as empty sets; results are sorted.
    assert_eq!(
        bulks_of(&call(&s, "sunion", &[a, b, missing])),
        vec![b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]
    );
    assert_eq!(bulks_of(&call(&s, "sinter", &[a, b])), vec![b"y".to_vec()]);
    assert_eq!(
        bulks_of(&call(&s, "sinter", &[a, missing])),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(bulks_of(&call(&s, "sdiff", &[a, b])), vec![b"x".to_vec()]);
    // Cross-slot rejection.
    assert_eq!(
        call(&s, "sunion", &[a, b"{u}noslot"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    // Non-set operand errors WRONGTYPE.
    set_raw(&s, b"{g}str", b"v");
    let wrong = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec();
    assert_eq!(call(&s, "sunion", &[a, b"{g}str"]), wrong);
    assert_eq!(call(&s, "smembers", &[b"{g}str"]), wrong);
    assert_eq!(call(&s, "scard", &[b"{g}str"]), wrong);
}

#[test]
fn set_algebra_store_variants() {
    let (_g, s) = shared_for("127.0.0.1:40318");
    let (a, b, dst, out) = (
        b"{g}a".as_slice(),
        b"{g}b".as_slice(),
        b"{g}dst".as_slice(),
        b"{g}out".as_slice(),
    );
    call(&s, "sadd", &[a, b"x", b"y"]);
    call(&s, "sadd", &[b, b"y", b"z"]);
    call(&s, "sadd", &[dst, b"stale"]);
    // SUNIONSTORE overwrites the destination wholesale.
    assert_eq!(int_of(&call(&s, "sunionstore", &[dst, a, b])), 3);
    assert_eq!(
        members(&s, dst),
        vec![b"x".to_vec(), b"y".to_vec(), b"z".to_vec()]
    );
    assert_eq!(int_of(&call(&s, "sinterstore", &[dst, a, b])), 1);
    assert_eq!(members(&s, dst), vec![b"y".to_vec()]);
    // SDIFFSTORE with destination == source (sources are read first).
    assert_eq!(int_of(&call(&s, "sdiffstore", &[dst, dst, b])), 0);
    // Empty result deletes the destination (empty set does not exist).
    assert_eq!(int_of(&call(&s, "exists", &[dst])), 0);
    // Storing into a brand-new destination works.
    assert_eq!(int_of(&call(&s, "sdiffstore", &[out, a, b])), 1);
    assert_eq!(members(&s, out), vec![b"x".to_vec()]);
    // Cross-slot (the destination counts too) and non-set destination.
    assert_eq!(
        call(&s, "sunionstore", &[b"{u}farkey", a]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    set_raw(&s, b"{g}str", b"v");
    assert_eq!(
        call(&s, "sunionstore", &[b"{g}str", a]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

/// Negative SPOP counts must error like zero: unchecked they wrap in
/// `n as u64` and pop the WHOLE set (regression guard).
#[test]
fn spop_negative_count_errors_and_keeps_set() {
    let (_g, s) = shared_for("127.0.0.1:40319");
    call(&s, "sadd", &[b"k", b"a", b"b", b"c"]);
    let err = b"-ERR value is out of range, must be positive\r\n".to_vec();
    assert_eq!(call(&s, "spop", &[b"k", b"-1"]), err);
    assert_eq!(call(&s, "spop", &[b"k", b"-5"]), err);
    // The set is untouched by both attempts.
    assert_eq!(int_of(&call(&s, "scard", &[b"k"])), 3);
    assert_eq!(
        members(&s, b"k"),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
}

/// SMOVE core semantics: move, missing member, WRONGTYPE on either side.
#[test]
fn smove_semantics_and_wrongtype() {
    let (_g, s) = shared_for("127.0.0.1:40320");
    let (src, dst) = (b"{g}s".as_slice(), b"{g}d".as_slice());
    call(&s, "sadd", &[src, b"a", b"b"]);
    call(&s, "sadd", &[dst, b"c"]);
    // Moved member changes both sides atomically.
    assert_eq!(int_of(&call(&s, "smove", &[src, dst, b"a"])), 1);
    assert_eq!(members(&s, src), vec![b"b".to_vec()]);
    assert_eq!(members(&s, dst), vec![b"a".to_vec(), b"c".to_vec()]);
    // Missing member replies 0 and changes nothing.
    assert_eq!(int_of(&call(&s, "smove", &[src, dst, b"zz"])), 0);
    assert_eq!(members(&s, src), vec![b"b".to_vec()]);
    assert_eq!(members(&s, dst), vec![b"a".to_vec(), b"c".to_vec()]);
    // A non-set source errors WRONGTYPE before any mutation.
    set_raw(&s, b"{g}str", b"v");
    assert_eq!(
        call(&s, "smove", &[b"{g}str", dst, b"c"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
    // A non-set destination errors WRONGTYPE too.
    assert_eq!(
        call(&s, "smove", &[src, b"{g}str", b"b"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
    // Neither error disturbed the operands.
    assert_eq!(members(&s, src), vec![b"b".to_vec()]);
    assert_eq!(members(&s, dst), vec![b"a".to_vec(), b"c".to_vec()]);
}

/// Cardinality lock: every has_member consumer (SADD dedup, SREM, SMOVE)
/// must keep SCARD exact. A storage error misread as "member absent"
/// would re-add an existing member and drift the meta count permanently
/// (read errors are not forceable in-process; this pins the success-path
/// arithmetic).
#[test]
fn set_cardinality_stays_exact_across_writes() {
    let (_g, s) = shared_for("127.0.0.1:40321");
    let key = b"{g}k".as_slice();
    assert_eq!(int_of(&call(&s, "sadd", &[key, b"a", b"b"])), 2);
    assert_eq!(int_of(&call(&s, "sadd", &[key, b"a", b"b", b"c"])), 1);
    assert_eq!(int_of(&call(&s, "scard", &[key])), 3);
    // Re-adding existing members never grows the cardinality.
    for _ in 0..5 {
        assert_eq!(int_of(&call(&s, "sadd", &[key, b"a", b"b", b"c"])), 0);
    }
    assert_eq!(int_of(&call(&s, "scard", &[key])), 3);
    // SREM over a mix of present and absent members is exact.
    assert_eq!(int_of(&call(&s, "srem", &[key, b"zz", b"a"])), 1);
    assert_eq!(int_of(&call(&s, "scard", &[key])), 2);
    // Round-trip SMOVE keeps both cardinalities exact.
    assert_eq!(int_of(&call(&s, "smove", &[key, b"{g}dst", b"b"])), 1);
    assert_eq!(int_of(&call(&s, "scard", &[key])), 1);
    assert_eq!(int_of(&call(&s, "scard", &[b"{g}dst"])), 1);
    assert_eq!(int_of(&call(&s, "smove", &[b"{g}dst", key, b"b"])), 1);
    assert_eq!(int_of(&call(&s, "scard", &[key])), 2);
    assert_eq!(int_of(&call(&s, "exists", &[b"{g}dst"])), 0);
    assert_eq!(members(&s, key), vec![b"b".to_vec(), b"c".to_vec()]);
}
