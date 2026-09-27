//! Hash READ family e2e through the real registry: HSETNX guard writes,
//! HMGET nulls-in-position, HEXISTS/HSTRLEN, HKEYS/HVALS in lexicographic
//! field order, and HRANDFIELD sampling (distinct/repeat/WITHVALUES).

mod common;

use common::lite::{call, shared_at};

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
fn hsetnx_sets_once_then_guards_the_field() {
    let (shared, _path) = shared_at("45101");
    assert_eq!(
        call(&shared, "hsetnx", &[b"h", b"f", b"one"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hget", &[b"h", b"f"]),
        b"$3\r\none\r\n".to_vec()
    );
    // Second HSETNX on the same field is a no-op: value and count stay.
    assert_eq!(
        call(&shared, "hsetnx", &[b"h", b"f", b"two"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hget", &[b"h", b"f"]),
        b"$3\r\none\r\n".to_vec()
    );
    assert_eq!(call(&shared, "hlen", &[b"h"]), b":1\r\n".to_vec());
    // Only the NAMED field is guarded; another field still lands.
    assert_eq!(
        call(&shared, "hsetnx", &[b"h", b"g", b"x"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(call(&shared, "hlen", &[b"h"]), b":2\r\n".to_vec());
    // Arity: short and long forms.
    assert_eq!(
        call(&shared, "hsetnx", &[b"h", b"f"]),
        b"-ERR wrong number of arguments for 'hsetnx' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hsetnx", &[b"h", b"f", b"v", b"x"]),
        b"-ERR wrong number of arguments for 'hsetnx' command\r\n".to_vec()
    );
    // A raw string key is refused before any write.
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(
        call(&shared, "hsetnx", &[b"str", b"f", b"v"]),
        WRONGTYPE.to_vec()
    );
}

#[test]
fn hmget_values_with_nulls_in_position() {
    let (shared, _path) = shared_at("45102");
    assert_eq!(
        call(&shared, "hset", &[b"h", b"a", b"1", b"b", b"two"]),
        b":2\r\n".to_vec()
    );
    // Order mirrors the requested fields; absent fields are nulls inline.
    assert_eq!(
        call(&shared, "hmget", &[b"h", b"b", b"nope", b"a"]),
        b"*3\r\n$3\r\ntwo\r\n$-1\r\n$1\r\n1\r\n".to_vec()
    );
    // Missing key: one null per requested field, never an empty array.
    assert_eq!(
        call(&shared, "hmget", &[b"gone", b"x", b"y"]),
        b"*2\r\n$-1\r\n$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hmget", &[b"h"]),
        b"-ERR wrong number of arguments for 'hmget' command\r\n".to_vec()
    );
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "hmget", &[b"str", b"f"]), WRONGTYPE.to_vec());
}

#[test]
fn hexists_and_hstrlen_presence_and_lengths() {
    let (shared, _path) = shared_at("45103");
    assert_eq!(
        call(&shared, "hset", &[b"h", b"f", b"hello", b"empty", b""]),
        b":2\r\n".to_vec()
    );
    assert_eq!(call(&shared, "hexists", &[b"h", b"f"]), b":1\r\n".to_vec());
    assert_eq!(
        call(&shared, "hexists", &[b"h", b"nope"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hexists", &[b"gone", b"f"]),
        b":0\r\n".to_vec()
    );
    // Byte length of the stored value; an empty value is 0, not absent.
    assert_eq!(call(&shared, "hstrlen", &[b"h", b"f"]), b":5\r\n".to_vec());
    assert_eq!(
        call(&shared, "hstrlen", &[b"h", b"empty"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hstrlen", &[b"h", b"nope"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hstrlen", &[b"gone", b"f"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hexists", &[b"h"]),
        b"-ERR wrong number of arguments for 'hexists' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hstrlen", &[b"h", b"f", b"x"]),
        b"-ERR wrong number of arguments for 'hstrlen' command\r\n".to_vec()
    );
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(
        call(&shared, "hexists", &[b"str", b"f"]),
        WRONGTYPE.to_vec()
    );
    assert_eq!(
        call(&shared, "hstrlen", &[b"str", b"f"]),
        WRONGTYPE.to_vec()
    );
}

#[test]
fn hkeys_hvals_walk_fields_in_lexicographic_order() {
    let (shared, _path) = shared_at("45104");
    assert_eq!(
        call(
            &shared,
            "hset",
            &[b"h", b"z", b"Z9", b"a", b"A1", b"m", b"M5"]
        ),
        b":3\r\n".to_vec()
    );
    // Insertion order (z,a,m) is irrelevant: the field range reads a,m,z.
    assert_eq!(
        call(&shared, "hkeys", &[b"h"]),
        b"*3\r\n$1\r\na\r\n$1\r\nm\r\n$1\r\nz\r\n".to_vec()
    );
    // HVALS pairs every value with its field's position.
    assert_eq!(
        call(&shared, "hvals", &[b"h"]),
        b"*3\r\n$2\r\nA1\r\n$2\r\nM5\r\n$2\r\nZ9\r\n".to_vec()
    );
    // Missing key: empty array for both names.
    assert_eq!(call(&shared, "hkeys", &[b"gone"]), b"*0\r\n".to_vec());
    assert_eq!(call(&shared, "hvals", &[b"gone"]), b"*0\r\n".to_vec());
    assert_eq!(
        call(&shared, "hkeys", &[]),
        b"-ERR wrong number of arguments for 'hkeys' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hvals", &[b"h", b"x"]),
        b"-ERR wrong number of arguments for 'hvals' command\r\n".to_vec()
    );
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "hkeys", &[b"str"]), WRONGTYPE.to_vec());
    assert_eq!(call(&shared, "hvals", &[b"str"]), WRONGTYPE.to_vec());
}

#[test]
fn hrandfield_missing_key_and_single_field_hash() {
    let (shared, _path) = shared_at("45105");
    // No count: null bulk when the key is missing; any count: empty array.
    assert_eq!(call(&shared, "hrandfield", &[b"gone"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&shared, "hrandfield", &[b"gone", b"3"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[b"gone", b"-3"]),
        b"*0\r\n".to_vec()
    );
    // A one-field hash pins every random draw to that field.
    assert_eq!(
        call(&shared, "hset", &[b"h", b"only", b"v1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[b"h"]),
        b"$4\r\nonly\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[b"h", b"1"]),
        b"*1\r\n$4\r\nonly\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[b"h", b"0"]),
        b"*0\r\n".to_vec()
    );
}

#[test]
fn hrandfield_counts_distinct_repeat_and_withvalues() {
    let (shared, _path) = shared_at("45106");
    assert_eq!(
        call(&shared, "hset", &[b"h", b"a", b"A", b"b", b"B", b"c", b"C"]),
        b":3\r\n".to_vec()
    );
    // Positive count above the cardinality: every DISTINCT field, unordered.
    let r = call(&shared, "hrandfield", &[b"h", b"10"]);
    assert!(r.starts_with(b"*3\r\n"), "clamped distinct header: {r:?}");
    let mut sorted = bulks(&r);
    sorted.sort();
    assert_eq!(sorted, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    // Negative count: |count| independent draws WITH repetition.
    let r = call(&shared, "hrandfield", &[b"h", b"-7"]);
    assert!(r.starts_with(b"*7\r\n"), "repeat draw header: {r:?}");
    assert!(
        bulks(&r)
            .iter()
            .all(|f| f == &b"a"[..] || f == &b"b"[..] || f == &b"c"[..]),
        "repeat draw members: {r:?}"
    );
    // WITHVALUES flattens [field, value] pairs after each pick.
    let r = call(&shared, "hrandfield", &[b"h", b"1", b"WITHVALUES"]);
    assert!(r.starts_with(b"*2\r\n"), "withvalues header: {r:?}");
    let pairs = bulks(&r);
    assert_eq!(pairs.len(), 2, "one field + one value: {r:?}");
    let paired = (pairs[0] == b"a".to_vec() && pairs[1] == b"A".to_vec())
        || (pairs[0] == b"b".to_vec() && pairs[1] == b"B".to_vec())
        || (pairs[0] == b"c".to_vec() && pairs[1] == b"C".to_vec());
    assert!(paired, "field/value pairing: {r:?}");
}

#[test]
fn hrandfield_error_paths() {
    let (shared, _path) = shared_at("45107");
    assert_eq!(
        call(&shared, "hset", &[b"h", b"f", b"v"]),
        b":1\r\n".to_vec()
    );
    // A non-integer count is refused (even the WITHVALUES keyword there).
    assert_eq!(
        call(&shared, "hrandfield", &[b"h", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[b"h", b"WITHVALUES"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    // A third arg must be exactly WITHVALUES.
    assert_eq!(
        call(&shared, "hrandfield", &[b"h", b"2", b"NOTANOPTION"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[b"h", b"1", b"WITHVALUES", b"x"]),
        b"-ERR wrong number of arguments for 'hrandfield' command\r\n".to_vec()
    );
    assert_eq!(
        call(&shared, "hrandfield", &[]),
        b"-ERR wrong number of arguments for 'hrandfield' command\r\n".to_vec()
    );
    assert_eq!(call(&shared, "set", &[b"str", b"v"]), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "hrandfield", &[b"str"]), WRONGTYPE.to_vec());
    assert_eq!(
        call(&shared, "hrandfield", &[b"str", b"2"]),
        WRONGTYPE.to_vec()
    );
}
