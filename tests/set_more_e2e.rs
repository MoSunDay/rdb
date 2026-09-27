//! More set family e2e through the real registry: SPOP (removing sampler
//! with count clamp), SRANDMEMBER (read-only twin), SSCAN hex-cursor
//! paging, SDIFF/SINTER sorted algebra with the CROSSSLOT gate, and
//! SMISMEMBER flag arrays.

mod common;

use common::lite::{call, shared_at};

const CROSSSLOT: &[u8] = b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n";
const WRONGTYPE: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";

/// Bulk payloads of a reply in reply order (array headers and ints skipped).
fn bulks(reply: &[u8]) -> Vec<Vec<u8>> {
    let s = String::from_utf8_lossy(reply);
    let mut out = Vec::new();
    let mut lines = s.split("\r\n");
    while let Some(head) = lines.next() {
        if let Some(len) = head.strip_prefix('$') {
            let len: usize = len.parse().unwrap();
            let body = lines.next().unwrap_or("");
            out.push(body.as_bytes()[..len.min(body.len())].to_vec());
        }
    }
    out
}

#[test]
fn spop_without_count_removes_one_member() {
    let (shared, _path) = shared_at("45201");
    // Single-member set: the draw is deterministic, the set disappears.
    assert_eq!(call(&shared, "sadd", &[b"s", b"m"]), b":1\r\n".to_vec());
    assert_eq!(call(&shared, "spop", &[b"s"]), b"$1\r\nm\r\n".to_vec());
    assert_eq!(call(&shared, "scard", &[b"s"]), b":0\r\n".to_vec());
    // Re-popping the (now missing) key answers null.
    assert_eq!(call(&shared, "spop", &[b"s"]), b"$-1\r\n".to_vec());
    // Multi-member set: any member may leave, exactly one does.
    assert_eq!(
        call(&shared, "sadd", &[b"s", b"a", b"b", b"c"]),
        b":3\r\n".to_vec()
    );
    let r = call(&shared, "spop", &[b"s"]);
    let got = bulks(&r);
    assert_eq!(got.len(), 1, "single bulk reply: {r:?}");
    assert!(
        got[0] == b"a".to_vec() || got[0] == b"b".to_vec() || got[0] == b"c".to_vec(),
        "member drawn: {r:?}"
    );
    assert_eq!(call(&shared, "scard", &[b"s"]), b":2\r\n".to_vec());
    assert_eq!(
        call(&shared, "sismember", &[b"s", got[0].as_slice()]),
        b":0\r\n".to_vec()
    );
}

#[test]
fn spop_with_count_clamps_and_empties_the_set() {
    let (shared, _path) = shared_at("45202");
    assert_eq!(
        call(&shared, "sadd", &[b"s", b"a", b"b", b"c"]),
        b":3\r\n".to_vec()
    );
    // count 2 pops two DISTINCT members (random order); one survives.
    let r = call(&shared, "spop", &[b"s", b"2"]);
    assert!(r.starts_with(b"*2\r\n"), "pop two header: {r:?}");
    let popped = bulks(&r);
    assert_ne!(popped[0], popped[1], "distinct picks: {r:?}");
    assert_eq!(call(&shared, "scard", &[b"s"]), b":1\r\n".to_vec());
    // A huge count clamps to the cardinality and deletes the family.
    let r = call(&shared, "spop", &[b"s", b"9223372036854775807"]);
    assert!(r.starts_with(b"*1\r\n"), "clamped pop: {r:?}");
    assert_eq!(call(&shared, "scard", &[b"s"]), b":0\r\n".to_vec());
    assert_eq!(call(&shared, "smembers", &[b"s"]), b"*0\r\n".to_vec());
    // count on a missing key: empty array (null is the no-count reply).
    assert_eq!(call(&shared, "spop", &[b"gone", b"5"]), b"*0\r\n".to_vec());
}

#[test]
fn spop_error_paths() {
    let (shared, _path) = shared_at("45203");
    assert_eq!(call(&shared, "sadd", &[b"s", b"a"]), b":1\r\n".to_vec());
    // Zero and negative counts are refused, never a whole-set wrap pop.
    assert_eq!(
        call(&shared, "spop", &[b"s", b"0"]),
        b"-ERR value is out of range, must be positive\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "spop", &[b"s", b"-2"]),
        b"-ERR value is out of range, must be positive\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "spop", &[b"s", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "spop", &[]),
        b"-ERR wrong number of arguments for 'spop' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "spop", &[b"s", b"1", b"x"]),
        b"-ERR wrong number of arguments for 'spop' command\r\n".to_vec()
    );
    // None of the error paths mutated the set.
    assert_eq!(call(&shared, "scard", &[b"s"]), b":1\r\n".to_vec());
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "spop", &[b"str"]), WRONGTYPE.to_vec());
}

#[test]
fn srandmember_samples_without_removing() {
    let (shared, _path) = shared_at("45204");
    assert_eq!(
        call(&shared, "sadd", &[b"s", b"a", b"b", b"c"]),
        b":3\r\n".to_vec()
    );
    // No count: one bulk member; the set is untouched.
    let r = call(&shared, "srandmember", &[b"s"]);
    let got = bulks(&r);
    assert_eq!(got.len(), 1, "one bulk reply: {r:?}");
    assert!(
        got[0] == b"a".to_vec() || got[0] == b"b".to_vec() || got[0] == b"c".to_vec(),
        "member drawn: {r:?}"
    );
    // Positive count above the cardinality: distinct members only.
    let r = call(&shared, "srandmember", &[b"s", b"9"]);
    assert!(r.starts_with(b"*3\r\n"), "distinct header: {r:?}");
    let mut sorted = bulks(&r);
    sorted.sort();
    assert_eq!(sorted, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    // Negative count: |count| independent draws, repeats allowed.
    let r = call(&shared, "srandmember", &[b"s", b"-8"]);
    assert!(r.starts_with(b"*8\r\n"), "repeat header: {r:?}");
    assert!(
        bulks(&r)
            .iter()
            .all(|m| m == &b"a"[..] || m == &b"b"[..] || m == &b"c"[..]),
        "repeat draws: {r:?}"
    );
    assert_eq!(call(&shared, "scard", &[b"s"]), b":3\r\n".to_vec());
    // Missing key: null bulk without count, empty array with one.
    assert_eq!(
        call(&shared, "srandmember", &[b"gone"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "srandmember", &[b"gone", b"4"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "srandmember", &[b"s", b"0"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "srandmember", &[b"s", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "srandmember", &[b"s", b"1", b"x"]),
        b"-ERR wrong number of arguments for 'srandmember' command\r\n".to_vec()
    );
}

#[test]
fn sscan_walks_pages_with_hex_cursors() {
    let (shared, _path) = shared_at("45205");
    assert_eq!(
        call(&shared, "sadd", &[b"s", b"c", b"a", b"b"]),
        b":3\r\n".to_vec()
    );
    // Default COUNT 10 takes everything in member (lexicographic) order.
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"0"]),
        b"*2\r\n$1\r\n0\r\n*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n".to_vec()
    );
    // COUNT 1: one member per page, cursor = hex of the last member.
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"0", b"COUNT", b"1"]),
        b"*2\r\n$2\r\n61\r\n*1\r\n$1\r\na\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"61", b"COUNT", b"1"]),
        b"*2\r\n$2\r\n62\r\n*1\r\n$1\r\nb\r\n".to_vec()
    );
    // A COUNT-limited page ends at the last member it RETURNED, even when
    // that member was the final one: the cursor is "63", not "0".
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"62", b"COUNT", b"1"]),
        b"*2\r\n$2\r\n63\r\n*1\r\n$1\r\nc\r\n".to_vec()
    );
    // One more step past the end finishes the walk: cursor "0", no members.
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"63", b"COUNT", b"1"]),
        b"*2\r\n$1\r\n0\r\n*0\r\n".to_vec()
    );
    // Resuming mid-range without COUNT returns the rest in one page.
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"61"]),
        b"*2\r\n$1\r\n0\r\n*2\r\n$1\r\nb\r\n$1\r\nc\r\n".to_vec()
    );
    // MATCH filters members; a fully consumed scan ends at cursor 0.
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"0", b"MATCH", b"b*"]),
        b"*2\r\n$1\r\n0\r\n*1\r\n$1\r\nb\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"0", b"MATCH", b"zz*"]),
        b"*2\r\n$1\r\n0\r\n*0\r\n".to_vec()
    );
    // Missing key: finished cursor, empty page.
    assert_eq!(
        call(&shared, "sscan", &[b"gone", b"0"]),
        b"*2\r\n$1\r\n0\r\n*0\r\n".to_vec()
    );
    // Arity, cursor decoding, COUNT bounds and syntax.
    assert_eq!(
        call(&shared, "sscan", &[b"s"]),
        b"-ERR wrong number of arguments for 'sscan' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"zz"]),
        b"-ERR invalid cursor\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"0", b"COUNT", b"0"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sscan", &[b"s", b"0", b"BOGUS"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "sscan", &[b"str", b"0"]), WRONGTYPE.to_vec());
}

#[test]
fn sdiff_and_sinter_sorted_algebra() {
    let (shared, _path) = shared_at("45206");
    assert_eq!(
        call(&shared, "sadd", &[b"{g}a", b"x", b"y", b"z"]),
        b":3\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sadd", &[b"{g}b", b"y", b"w"]),
        b":2\r\n".to_vec()
    );
    // Results are SORTED (Redis leaves the order unspecified).
    assert_eq!(
        call(&shared, "sdiff", &[b"{g}a", b"{g}b"]),
        b"*2\r\n$1\r\nx\r\n$1\r\nz\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sinter", &[b"{g}a", b"{g}b"]),
        b"*1\r\n$1\r\ny\r\n".to_vec()
    );
    // Single operand: SDIFF is the set itself; SINTER seeds from the
    // only operand, so it is also the whole set (sorted).
    assert_eq!(
        call(&shared, "sdiff", &[b"{g}a"]),
        b"*3\r\n$1\r\nx\r\n$1\r\ny\r\n$1\r\nz\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sinter", &[b"{g}a"]),
        b"*3\r\n$1\r\nx\r\n$1\r\ny\r\n$1\r\nz\r\n".to_vec()
    );
    // Missing operands read as empty sets: diff keeps, inter drops.
    assert_eq!(
        call(&shared, "sdiff", &[b"{g}a", b"{g}none"]),
        b"*3\r\n$1\r\nx\r\n$1\r\ny\r\n$1\r\nz\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sinter", &[b"{g}a", b"{g}none"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(call(&shared, "sinter", &[b"{g}none"]), b"*0\r\n".to_vec());
    // Three-way diff: first minus every later operand.
    assert_eq!(
        call(&shared, "sadd", &[b"{g}c", b"x", b"q"]),
        b":2\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sdiff", &[b"{g}a", b"{g}b", b"{g}c"]),
        b"*1\r\n$1\r\nz\r\n".to_vec()
    );
    // Arity, CROSSSLOT (untagged keys in different slots) and WRONGTYPE.
    assert_eq!(
        call(&shared, "sdiff", &[]),
        b"-ERR wrong number of arguments for 'sdiff' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sinter", &[]),
        b"-ERR wrong number of arguments for 'sinter' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sdiff", &[b"{g}a", b"{u}far"]),
        CROSSSLOT.to_vec()
    );
    assert_eq!(
        call(&shared, "sinter", &[b"{g}a", b"{u}far"]),
        CROSSSLOT.to_vec()
    );
    assert_eq!(
        call(&shared, "set", &[b"{g}str", b"v"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "sinter", &[b"{g}a", b"{g}str"]),
        WRONGTYPE.to_vec()
    );
    assert_eq!(
        call(&shared, "sdiff", &[b"{g}str", b"{g}a"]),
        WRONGTYPE.to_vec()
    );
    // Reads never mutated the operands.
    assert_eq!(call(&shared, "scard", &[b"{g}a"]), b":3\r\n".to_vec());
}

#[test]
fn smismember_flags_in_request_order() {
    let (shared, _path) = shared_at("45207");
    assert_eq!(
        call(&shared, "sadd", &[b"s", b"a", b"b"]),
        b":2\r\n".to_vec()
    );
    // One 0/1 flag per requested member, positionally.
    assert_eq!(
        call(&shared, "smismember", &[b"s", b"b", b"nope", b"a"]),
        b"*3\r\n:1\r\n:0\r\n:1\r\n".to_vec()
    );
    // Missing key: all zeros, the array shape still mirrors the args.
    assert_eq!(
        call(&shared, "smismember", &[b"gone", b"a", b"b"]),
        b"*2\r\n:0\r\n:0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "smismember", &[b"s"]),
        b"-ERR wrong number of arguments for 'smismember' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "smismember", &[]),
        b"-ERR wrong number of arguments for 'smismember' command\r\n".to_vec()
    );
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(
        call(&shared, "smismember", &[b"str", b"a"]),
        WRONGTYPE.to_vec()
    );
    // Pure read: flags do not disturb the set.
    assert_eq!(call(&shared, "scard", &[b"s"]), b":2\r\n".to_vec());
}
