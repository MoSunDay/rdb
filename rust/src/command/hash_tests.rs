//! Handler-level tests for the hash commands: crate-wide store lock,
//! fresh `Shared` per test, dispatch through the command registry
//! (`command::lookup`) so the registered names are exercised too.

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

fn bulk_of(reply: &[u8]) -> Vec<u8> {
    // Single-value replies (`$len\r\n<bytes>\r\n`); the test_reader only
    // parses arrays, so decode the bulk frame by hand.
    let text_end = reply.iter().position(|&b| b == b'\n').expect("line ends");
    let len: usize = std::str::from_utf8(&reply[1..text_end - 1])
        .expect("bulk header")
        .parse()
        .expect("bulk length");
    reply[text_end + 1..text_end + 1 + len].to_vec()
}

fn set_raw(shared: &Shared, key: &[u8], val: &[u8]) {
    crate::store::set(&shared.store, PREFIX, key, val).expect("raw set");
}

#[test]
fn hset_multi_pair_counts_and_arity() {
    let (_g, s) = shared_for("127.0.0.1:40301");
    assert_eq!(int_of(&call(&s, "hset", &[b"k", b"a", b"1"])), 1);
    // Update + insert in one call: only NEW fields are counted.
    assert_eq!(
        int_of(&call(&s, "hset", &[b"k", b"a", b"2", b"b", b"22"])),
        1
    );
    assert_eq!(bulk_of(&call(&s, "hget", &[b"k", b"a"])), b"2".to_vec());
    // Odd pair count is an arity error.
    assert_eq!(
        call(&s, "hset", &[b"k", b"a"]),
        b"-ERR wrong number of arguments for 'hset' command\r\n".to_vec()
    );
}

#[test]
fn hsetnx_then_hdel_deletes_empty_key() {
    let (_g, s) = shared_for("127.0.0.1:40302");
    assert_eq!(int_of(&call(&s, "hsetnx", &[b"k", b"f", b"v"])), 1);
    assert_eq!(int_of(&call(&s, "hsetnx", &[b"k", b"f", b"other"])), 0);
    assert_eq!(bulk_of(&call(&s, "hget", &[b"k", b"f"])), b"v".to_vec());
    // Deleting the LAST field removes the whole key (empty hash does not
    // exist); a second HDEL reports 0.
    assert_eq!(int_of(&call(&s, "hdel", &[b"k", b"f"])), 1);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    assert_eq!(call(&s, "hget", &[b"k", b"f"]), b"$-1\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "hdel", &[b"k", b"f"])), 0);
}

#[test]
fn hmget_mixed_nulls_in_order() {
    let (_g, s) = shared_for("127.0.0.1:40303");
    call(&s, "hset", &[b"k", b"there", b"hello"]);
    assert_eq!(
        call(&s, "hmget", &[b"k", b"missing", b"there"]),
        b"*2\r\n$-1\r\n$5\r\nhello\r\n".to_vec()
    );
}

#[test]
fn hlen_hexists_hstrlen() {
    let (_g, s) = shared_for("127.0.0.1:40304");
    call(&s, "hset", &[b"k", b"one", b"1", b"two", b"22"]);
    assert_eq!(int_of(&call(&s, "hlen", &[b"k"])), 2);
    assert_eq!(int_of(&call(&s, "hexists", &[b"k", b"one"])), 1);
    assert_eq!(int_of(&call(&s, "hexists", &[b"k", b"zzz"])), 0);
    assert_eq!(int_of(&call(&s, "hstrlen", &[b"k", b"two"])), 2);
    assert_eq!(int_of(&call(&s, "hstrlen", &[b"k", b"zzz"])), 0);
    // Missing key: HLEN 0, HEXISTS 0, HSTRLEN 0.
    assert_eq!(int_of(&call(&s, "hlen", &[b"none"])), 0);
    assert_eq!(int_of(&call(&s, "hexists", &[b"none", b"f"])), 0);
}

#[test]
fn hgetall_hkeys_hvals_ordered() {
    let (_g, s) = shared_for("127.0.0.1:40305");
    call(&s, "hset", &[b"k", b"b", b"2", b"a", b"1"]);
    let fields = test_reader::parse(&call(&s, "hgetall", &[b"k"]));
    let flat: Vec<Vec<u8>> = fields.iter().map(test_reader::bulk).collect();
    assert_eq!(
        flat,
        vec![b"a".to_vec(), b"1".to_vec(), b"b".to_vec(), b"2".to_vec()]
    );
    let keys = test_reader::parse(&call(&s, "hkeys", &[b"k"]));
    let flat: Vec<Vec<u8>> = keys.iter().map(test_reader::bulk).collect();
    assert_eq!(flat, vec![b"a".to_vec(), b"b".to_vec()]);
    let vals = test_reader::parse(&call(&s, "hvals", &[b"k"]));
    let flat: Vec<Vec<u8>> = vals.iter().map(test_reader::bulk).collect();
    assert_eq!(flat, vec![b"1".to_vec(), b"2".to_vec()]);
    assert_eq!(call(&s, "hgetall", &[b"none"]), b"*0\r\n".to_vec());
}

#[test]
fn hincrby_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40306");
    assert_eq!(int_of(&call(&s, "hincrby", &[b"k", b"n", b"5"])), 5);
    assert_eq!(int_of(&call(&s, "hincrby", &[b"k", b"n", b"-8"])), -3);
    // Non-integer delta and non-integer existing value both error.
    assert_eq!(
        call(&s, "hincrby", &[b"k", b"n", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    call(&s, "hset", &[b"k", b"str", b"nope"]);
    assert_eq!(
        call(&s, "hincrby", &[b"k", b"str", b"1"]),
        b"-ERR hash value is not an integer\r\n".to_vec()
    );
    call(&s, "hset", &[b"k", b"max", b"9223372036854775807"]);
    assert_eq!(
        call(&s, "hincrby", &[b"k", b"max", b"1"]),
        b"-ERR increment or decrement would overflow\r\n".to_vec()
    );
    // Bug fix: an empty-string field value is not an integer either.
    call(&s, "hset", &[b"k", b"empty", b""]);
    assert_eq!(
        call(&s, "hincrby", &[b"k", b"empty", b"1"]),
        b"-ERR hash value is not an integer\r\n".to_vec()
    );
    // Missing fields still start from 0.
    assert_eq!(int_of(&call(&s, "hincrby", &[b"k", b"fresh", b"1"])), 1);
}

#[test]
fn hincrbyfloat_roundtrip_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40307");
    assert_eq!(
        bulk_of(&call(&s, "hincrbyfloat", &[b"k", b"n", b"1.5"])),
        b"1.5".to_vec()
    );
    assert_eq!(
        bulk_of(&call(&s, "hincrbyfloat", &[b"k", b"n", b"2.25"])),
        b"3.75".to_vec()
    );
    assert_eq!(
        call(&s, "hincrbyfloat", &[b"k", b"n", b"zz"]),
        b"-ERR value is not a valid float\r\n".to_vec()
    );
    call(&s, "hset", &[b"k", b"str", b"nope"]);
    assert_eq!(
        call(&s, "hincrbyfloat", &[b"k", b"str", b"1"]),
        b"-ERR hash value is not a float\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "hincrbyfloat", &[b"k", b"n", b"1e309"]),
        b"-ERR increment would produce NaN or Infinity\r\n".to_vec()
    );
}

#[test]
fn hrandfield_count_semantics() {
    let (_g, s) = shared_for("127.0.0.1:40308");
    call(&s, "hset", &[b"k", b"a", b"1", b"b", b"2", b"c", b"3"]);
    // No count: exactly one field, from the key's fields.
    let one = bulk_of(&call(&s, "hrandfield", &[b"k"]));
    assert!(one == b"a" || one == b"b" || one == b"c");
    // count 2: distinct fields; count > card: all 3.
    let two: Vec<Vec<u8>> = test_reader::parse(&call(&s, "hrandfield", &[b"k", b"2"]))
        .iter()
        .map(test_reader::bulk)
        .collect();
    assert_eq!(two.len(), 2);
    assert_ne!(two[0], two[1]);
    let all: Vec<Vec<u8>> = test_reader::parse(&call(&s, "hrandfield", &[b"k", b"10"]))
        .iter()
        .map(test_reader::bulk)
        .collect();
    assert_eq!(all.len(), 3);
    // Negative count REPEATS and ignores cardinality.
    let rep: Vec<Vec<u8>> = test_reader::parse(&call(&s, "hrandfield", &[b"k", b"-7"]))
        .iter()
        .map(test_reader::bulk)
        .collect();
    assert_eq!(rep.len(), 7);
    // WITHVALUES flattens [f, v, ...].
    let pairs: Vec<Vec<u8>> =
        test_reader::parse(&call(&s, "hrandfield", &[b"k", b"2", b"WITHVALUES"]))
            .iter()
            .map(test_reader::bulk)
            .collect();
    assert_eq!(pairs.len(), 4);
    // Missing key: null / empty array.
    assert_eq!(call(&s, "hrandfield", &[b"none"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "hrandfield", &[b"none", b"3"]), b"*0\r\n".to_vec());
}

#[test]
fn hscan_pages_with_count_and_match() {
    let (_g, s) = shared_for("127.0.0.1:40309");
    let mut argv = vec![b"k".to_vec()];
    for (i, f) in [b"f1", b"f2", b"f3", b"f4", b"f5"].iter().enumerate() {
        argv.push(f.to_vec());
        argv.push(i.to_string().into_bytes());
    }
    call(
        &s,
        "hset",
        &argv.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
    );

    // Walk the cursor chain: 5 fields, COUNT 2 -> 2 + 2 + 1.
    let mut cursor = b"0".to_vec();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;
    loop {
        let page = test_reader::parse(&call(&s, "hscan", &[b"k", &cursor, b"COUNT", b"2"]));
        cursor = test_reader::bulk(&page[0]);
        let Frame::Array(items) = &page[1] else {
            panic!("items array");
        };
        for f in items {
            seen.push(test_reader::bulk(f));
        }
        pages += 1;
        if cursor == b"0" {
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(
        seen,
        vec![
            b"f1".to_vec(),
            b"f2".to_vec(),
            b"f3".to_vec(),
            b"f4".to_vec(),
            b"f5".to_vec()
        ]
    );

    // Deviation from Redis: HSCAN returns FIELDS only unless WITHVALUES is
    // given (WITHVALUES flattens [f, v, ...] rather than nesting pairs).
    let page = test_reader::parse(&call(&s, "hscan", &[b"k", b"0", b"MATCH", b"f[13]"]));
    let Frame::Array(items) = &page[1] else {
        panic!("items array");
    };
    let flat: Vec<Vec<u8>> = items.iter().map(test_reader::bulk).collect();
    assert_eq!(flat, vec![b"f1".to_vec(), b"f3".to_vec()]);
    let page = test_reader::parse(&call(
        &s,
        "hscan",
        &[b"k", b"0", b"MATCH", b"f[13]", b"WITHVALUES"],
    ));
    let Frame::Array(items) = &page[1] else {
        panic!("items array");
    };
    let flat: Vec<Vec<u8>> = items.iter().map(test_reader::bulk).collect();
    assert_eq!(
        flat,
        vec![b"f1".to_vec(), b"0".to_vec(), b"f3".to_vec(), b"2".to_vec()]
    );
    assert_eq!(
        call(&s, "hscan", &[b"k", b"zz", b"COUNT", b"2"]),
        b"-ERR invalid cursor\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "hscan", &[b"k", b"0", b"BOGUS"]),
        b"-ERR syntax error\r\n".to_vec()
    );
}

#[test]
fn hscan_pages_over_an_empty_field() {
    let (_g, s) = shared_for("127.0.0.1:40513");
    let argv: Vec<Vec<u8>> = vec![
        b"k".to_vec(),
        Vec::new(),
        b"0".to_vec(),
        b"a".to_vec(),
        b"1".to_vec(),
        b"b".to_vec(),
        b"2".to_vec(),
    ];
    call(
        &s,
        "hset",
        &argv.iter().map(|v| v.as_slice()).collect::<Vec<_>>(),
    );
    // COUNT 1 puts the empty field on a page boundary: the hex cursor of
    // "" is "" itself, which must resume STRICTLY AFTER "" — a restart
    // there would loop forever, and the old empty-bytes sentinel misread
    // it as "done", silently dropping the remaining fields.
    let mut cursor = b"0".to_vec();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    let mut pages = 0;
    loop {
        let page = test_reader::parse(&call(&s, "hscan", &[b"k", &cursor, b"COUNT", b"1"]));
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
}

#[test]
fn hash_commands_reject_string_key() {
    let (_g, s) = shared_for("127.0.0.1:40310");
    set_raw(&s, b"str", b"v");
    let wrong = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec();
    assert_eq!(call(&s, "hset", &[b"str", b"f", b"v"]), wrong);
    assert_eq!(call(&s, "hgetall", &[b"str"]), wrong);
    assert_eq!(call(&s, "hincrby", &[b"str", b"f", b"1"]), wrong);
    assert_eq!(call(&s, "hscan", &[b"str", b"0"]), wrong);
}
