//! Handler-level tests for the list commands: crate-wide store lock,
//! fresh `Shared` per test, dispatch through the command registry so the
//! registered names are exercised too. Ports 40410+ (40401-40405 are
//! taken by the migrate tests).

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

fn elems(shared: &Shared, key: &[u8]) -> Vec<Vec<u8>> {
    bulks_of(&call(shared, "lrange", &[key, b"0", b"-1"]))
}

fn set_raw(shared: &Shared, key: &[u8], val: &[u8]) {
    crate::store::set(&shared.store, PREFIX, key, val).expect("raw set");
}

#[test]
fn push_arity_and_pushx_on_missing() {
    let (_g, s) = shared_for("127.0.0.1:40410");
    assert_eq!(
        call(&s, "lpush", &[b"k"]),
        b"-ERR wrong number of arguments for 'lpush' command\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "rpush", &[b"k"]),
        b"-ERR wrong number of arguments for 'rpush' command\r\n".to_vec()
    );
    // LPUSHX/RPUSHX never create: 0 on a missing key, and no key after.
    assert_eq!(int_of(&call(&s, "lpushx", &[b"k", b"a"])), 0);
    assert_eq!(int_of(&call(&s, "rpushx", &[b"k", b"a"])), 0);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
}

#[test]
fn push_order_and_llen() {
    let (_g, s) = shared_for("127.0.0.1:40411");
    // LPUSH a b c => c b a (head first).
    assert_eq!(int_of(&call(&s, "lpush", &[b"k", b"a", b"b", b"c"])), 3);
    assert_eq!(
        elems(&s, b"k"),
        vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]
    );
    // RPUSH appends at the tail.
    assert_eq!(int_of(&call(&s, "rpush", &[b"k", b"z"])), 4);
    assert_eq!(elems(&s, b"k").last().unwrap(), b"z");
    assert_eq!(int_of(&call(&s, "llen", &[b"k"])), 4);
    assert_eq!(int_of(&call(&s, "llen", &[b"none"])), 0);
    set_raw(&s, b"str", b"v");
    assert_eq!(
        call(&s, "llen", &[b"str"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
    // LPUSHX/RPUSHX on an existing list append in their directions.
    assert_eq!(int_of(&call(&s, "lpushx", &[b"k", b"head"])), 5);
    assert_eq!(elems(&s, b"k").first().unwrap(), b"head");
    assert_eq!(int_of(&call(&s, "rpushx", &[b"k", b"tail"])), 6);
    assert_eq!(elems(&s, b"k").last().unwrap(), b"tail");
}

#[test]
fn lrange_clamps_and_missing() {
    let (_g, s) = shared_for("127.0.0.1:40412");
    call(&s, "rpush", &[b"k", b"a", b"b", b"c", b"d"]);
    assert_eq!(
        elems(&s, b"k"),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // Negative indices count from the tail; overlong windows clamp.
    let tail = call(&s, "lrange", &[b"k", b"-2", b"-1"]);
    assert_eq!(bulks_of(&tail), vec![b"c".to_vec(), b"d".to_vec()]);
    assert_eq!(call(&s, "lrange", &[b"k", b"5", b"10"]), b"*0\r\n".to_vec());
    assert_eq!(call(&s, "lrange", &[b"k", b"2", b"1"]), b"*0\r\n".to_vec());
    assert_eq!(
        call(&s, "lrange", &[b"none", b"0", b"-1"]),
        b"*0\r\n".to_vec()
    );
    // Bad index values are integer errors.
    assert_eq!(
        call(&s, "lrange", &[b"k", b"x", b"-1"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn lindex_positions() {
    let (_g, s) = shared_for("127.0.0.1:40413");
    call(&s, "rpush", &[b"k", b"a", b"b", b"c"]);
    assert_eq!(bulk_of(&call(&s, "lindex", &[b"k", b"0"])), b"a".to_vec());
    assert_eq!(bulk_of(&call(&s, "lindex", &[b"k", b"-1"])), b"c".to_vec());
    assert_eq!(bulk_of(&call(&s, "lindex", &[b"k", b"1"])), b"b".to_vec());
    assert_eq!(call(&s, "lindex", &[b"k", b"3"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "lindex", &[b"k", b"-4"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "lindex", &[b"none", b"0"]), b"$-1\r\n".to_vec());
    assert_eq!(
        call(&s, "lindex", &[b"k", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn lset_ok_missing_and_out_of_range() {
    let (_g, s) = shared_for("127.0.0.1:40414");
    call(&s, "rpush", &[b"k", b"a", b"b"]);
    assert_eq!(call(&s, "lset", &[b"k", b"0", b"z"]), b"+OK\r\n".to_vec());
    assert_eq!(elems(&s, b"k"), vec![b"z".to_vec(), b"b".to_vec()]);
    assert_eq!(call(&s, "lset", &[b"k", b"-1", b"y"]), b"+OK\r\n".to_vec());
    assert_eq!(
        call(&s, "lset", &[b"none", b"0", b"v"]),
        b"-ERR no such key\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "lset", &[b"k", b"9", b"v"]),
        b"-ERR index out of range\r\n".to_vec()
    );
    set_raw(&s, b"str", b"v");
    assert_eq!(
        call(&s, "lset", &[b"str", b"0", b"v"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn lpop_single_and_drain_deletes_key() {
    let (_g, s) = shared_for("127.0.0.1:40415");
    call(&s, "rpush", &[b"k", b"a", b"b", b"c"]);
    assert_eq!(bulk_of(&call(&s, "lpop", &[b"k"])), b"a".to_vec());
    // Draining the last element removes the key entirely.
    assert_eq!(bulk_of(&call(&s, "lpop", &[b"k"])), b"b".to_vec());
    assert_eq!(bulk_of(&call(&s, "lpop", &[b"k"])), b"c".to_vec());
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    assert_eq!(call(&s, "lpop", &[b"k"]), b"$-1\r\n".to_vec());
}

#[test]
fn lpop_count_variants_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40416");
    call(&s, "rpush", &[b"k", b"a", b"b", b"c"]);
    // Count 0 -> empty array; negative -> error; bad int -> error.
    assert_eq!(call(&s, "lpop", &[b"k", b"0"]), b"*0\r\n".to_vec());
    assert_eq!(
        call(&s, "lpop", &[b"k", b"-1"]),
        b"-ERR value is out of range, must be positive\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "lpop", &[b"k", b"x"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    // Asking for more than remains returns everything left, then the
    // key is gone.
    assert_eq!(
        bulks_of(&call(&s, "lpop", &[b"k", b"99"])),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    assert_eq!(call(&s, "lpop", &[b"none", b"2"]), b"*0\r\n".to_vec());
}

#[test]
fn rpop_mirrors_lpop() {
    let (_g, s) = shared_for("127.0.0.1:40417");
    call(&s, "rpush", &[b"k", b"a", b"b", b"c"]);
    assert_eq!(bulk_of(&call(&s, "rpop", &[b"k"])), b"c".to_vec());
    let pair = call(&s, "rpop", &[b"k", b"2"]);
    assert_eq!(bulks_of(&pair), vec![b"b".to_vec(), b"a".to_vec()]);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
}

#[test]
fn lrem_counts_directions_and_missing() {
    let (_g, s) = shared_for("127.0.0.1:40418");
    call(&s, "rpush", &[b"k", b"a", b"x", b"b", b"x", b"c", b"x"]);
    // count 0 removes every occurrence.
    assert_eq!(int_of(&call(&s, "lrem", &[b"k", b"0", b"x"])), 3);
    assert_eq!(
        elems(&s, b"k"),
        vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    // Positive counts walk head-first; negative tail-first.
    call(&s, "rpush", &[b"j", b"x", b"1", b"x", b"2", b"x"]);
    assert_eq!(int_of(&call(&s, "lrem", &[b"j", b"2", b"x"])), 2);
    assert_eq!(
        elems(&s, b"j"),
        vec![b"1".to_vec(), b"2".to_vec(), b"x".to_vec()]
    );
    assert_eq!(int_of(&call(&s, "lrem", &[b"j", b"-1", b"x"])), 1);
    assert_eq!(elems(&s, b"j"), vec![b"1".to_vec(), b"2".to_vec()]);
    assert_eq!(int_of(&call(&s, "lrem", &[b"none", b"0", b"x"])), 0);
    // Removing everything drops the key.
    assert_eq!(int_of(&call(&s, "lrem", &[b"j", b"0", b"1"])), 1);
    assert_eq!(int_of(&call(&s, "lrem", &[b"j", b"0", b"2"])), 1);
    assert_eq!(int_of(&call(&s, "exists", &[b"j"])), 0);
    assert_eq!(
        call(&s, "lrem", &[b"k", b"x", b"a"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn ltrim_keeps_window_or_deletes() {
    let (_g, s) = shared_for("127.0.0.1:40419");
    call(&s, "rpush", &[b"k", b"a", b"b", b"c", b"d", b"e"]);
    assert_eq!(call(&s, "ltrim", &[b"k", b"1", b"3"]), b"+OK\r\n".to_vec());
    assert_eq!(
        elems(&s, b"k"),
        vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    // An empty selection removes the key.
    assert_eq!(call(&s, "ltrim", &[b"k", b"5", b"6"]), b"+OK\r\n".to_vec());
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    // Trimming a missing key is still OK.
    assert_eq!(
        call(&s, "ltrim", &[b"none", b"0", b"1"]),
        b"+OK\r\n".to_vec()
    );
}

#[path = "list_more_tests.rs"]
mod list_more_tests;
