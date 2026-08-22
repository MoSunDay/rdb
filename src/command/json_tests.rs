//! Handler-level tests for the json commands: crate-wide store lock,
//! fresh `Shared` per test, dispatch through the command registry
//! (`command::lookup`) so the registered names are exercised too.

use crate::command::test_ctx;
use crate::command::Handler;
use crate::state::{testutil, Shared};

pub(super) const PREFIX: &[u8] = b"70/";

pub(super) fn shared_for(bind: &str) -> (std::sync::MutexGuard<'static, ()>, Shared) {
    let guard = crate::command::string::TEST_STORE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut conf = testutil::test_config();
    conf.bind = bind.to_string();
    (guard, testutil::shared_with(conf))
}

pub(super) fn call(shared: &Shared, name: &str, args: &[&[u8]]) -> Vec<u8> {
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

pub(super) fn int_of(reply: &[u8]) -> i64 {
    let text = String::from_utf8(reply.to_vec()).unwrap();
    text.trim_start_matches(':').trim_end().parse().unwrap()
}

pub(super) fn bulk_of(reply: &[u8]) -> Vec<u8> {
    // Single-value replies (`$len\r\n<bytes>\r\n`); the test_reader only
    // parses arrays, so decode the bulk frame by hand.
    let text_end = reply.iter().position(|&b| b == b'\n').expect("line ends");
    let len: usize = std::str::from_utf8(&reply[1..text_end - 1])
        .expect("bulk header")
        .parse()
        .expect("bulk length");
    reply[text_end + 1..text_end + 1 + len].to_vec()
}

#[test]
fn set_root_nested_autocreate_and_conditions() {
    let (_g, s) = shared_for("127.0.0.1:40701");
    assert_eq!(
        call(&s, "json.set", &[b"k", b".", b"{\"a\":1}"]),
        b"+OK\r\n".to_vec()
    );
    // Nested path auto-creates the missing intermediate object.
    assert_eq!(
        call(&s, "json.set", &[b"k", b".x.y.deep", b"[1]"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k"])),
        br#"{"a":1,"x":{"y":{"deep":[1]}}}"#.to_vec()
    );
    // NX on an existing key / XX on a missing key -> nil bulk.
    assert_eq!(
        call(&s, "json.set", &[b"k", b".", b"1", b"NX"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.set", &[b"nope", b".", b"1", b"XX"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.set", &[b"nope", b".", b"1", b"NADA"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.set", &[b"nope", b".", b"{bad"]),
        b"-ERR invalid JSON\r\n".to_vec()
    );
    // Wrong-type key and bad path syntax.
    crate::store::set(&s.store, PREFIX, b"raw", b"x").expect("raw set");
    assert_eq!(
        call(&s, "json.set", &[b"raw", b".", b"1"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.set", &[b"k", b"..a", b"1"]),
        b"-ERR wrong static path\r\n".to_vec()
    );
}

#[test]
fn get_roundtrip_is_byte_stable_and_multipath() {
    let (_g, s) = shared_for("127.0.0.1:40702");
    // Insertion order survives: no re-sorting of object keys.
    call(&s, "json.set", &[b"k", b".", b"{\"b\":1,\"a\":2}"]);
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k"])),
        br#"{"b":1,"a":2}"#.to_vec()
    );
    call(&s, "json.set", &[b"k", b"['odd.key']", b"7"]);
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k", b"['odd.key']"])),
        b"7".to_vec()
    );
    // Multi-path: flat array, one entry per path, null for the missing.
    assert_eq!(
        call(&s, "json.get", &[b"k", b".a", b".b", b".zzz"]),
        b"*3\r\n$1\r\n2\r\n$1\r\n1\r\n$-1\r\n".to_vec()
    );
    assert_eq!(call(&s, "json.get", &[b"gone"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "json.get", &[b"k", b".nope"]), b"$-1\r\n".to_vec());
}

#[test]
fn del_paths_and_root() {
    let (_g, s) = shared_for("127.0.0.1:40703");
    call(&s, "json.set", &[b"k", b".", b"{\"a\":{\"b\":1},\"c\":2}"]);
    assert_eq!(int_of(&call(&s, "json.del", &[b"k", b".a.b"])), 1);
    assert_eq!(int_of(&call(&s, "json.del", &[b"k", b".a.b"])), 0);
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k"])),
        br#"{"a":{},"c":2}"#.to_vec()
    );
    // Array removal shifts; out-of-range and missing paths answer 0.
    call(&s, "json.set", &[b"k", b".arr", b"[1,2,3]"]);
    assert_eq!(int_of(&call(&s, "json.del", &[b"k", b".arr[1]"])), 1);
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k", b".arr"])),
        b"[1,3]".to_vec()
    );
    assert_eq!(int_of(&call(&s, "json.del", &[b"k", b".arr[9]"])), 0);
    assert_eq!(int_of(&call(&s, "json.del", &[b"k", b".nope"])), 0);
    assert_eq!(int_of(&call(&s, "json.forget", &[b"k"])), 1);
    assert_eq!(int_of(&call(&s, "json.del", &[b"k"])), 0);
    assert_eq!(int_of(&call(&s, "exists", &[b"k"])), 0);
    // Root path spelling also deletes the whole key.
    call(&s, "json.set", &[b"k2", b".", b"1"]);
    assert_eq!(int_of(&call(&s, "json.del", &[b"k2", b".x"])), 0);
}

#[test]
fn type_names_integer_vs_number() {
    let (_g, s) = shared_for("127.0.0.1:40704");
    call(
        &s,
        "json.set",
        &[
            b"k",
            b".",
            br#"{"o":{},"a":[],"s":"x","i":3,"f":3.5,"b":true,"n":null}"#,
        ],
    );
    for (path, want) in [
        (".o", "+object\r\n"),
        (".a", "+array\r\n"),
        (".s", "+string\r\n"),
        (".i", "+integer\r\n"),
        (".f", "+number\r\n"),
        (".b", "+boolean\r\n"),
        (".n", "+null\r\n"),
        (".", "+object\r\n"),
    ] {
        assert_eq!(
            call(&s, "json.type", &[b"k", path.as_bytes()]),
            want.as_bytes().to_vec(),
            "path {path}"
        );
    }
    assert_eq!(call(&s, "json.type", &[b"k", b".zzz"]), b"$-1\r\n".to_vec());
    assert_eq!(call(&s, "json.type", &[b"gone"]), b"$-1\r\n".to_vec());
}

#[test]
fn mget_multi_key_and_crossslot() {
    let (_g, s) = shared_for("127.0.0.1:40705");
    call(&s, "json.set", &[b"{t}a", b".", b"{\"v\":1}"]);
    call(&s, "json.set", &[b"{t}b", b".", b"[2]"]);
    assert_eq!(
        call(&s, "json.mget", &[b"{t}a", b"{t}b", b"{t}c", b".v"]),
        b"*3\r\n$1\r\n1\r\n$-1\r\n$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.mget", &[b"{t}a", b"other", b".v"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    crate::store::set(&s.store, PREFIX, b"{t}s", b"x").expect("raw set");
    assert_eq!(
        call(&s, "json.mget", &[b"{t}a", b"{t}s", b".v"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n".to_vec()
    );
}

#[test]
fn strappend_and_strlen() {
    let (_g, s) = shared_for("127.0.0.1:40706");
    call(&s, "json.set", &[b"k", b".", br#"{"s":"he"}"#]);
    assert_eq!(
        int_of(&call(&s, "json.strappend", &[b"k", b".s", b"\"llo\""])),
        5
    );
    assert_eq!(
        bulk_of(&call(&s, "json.get", &[b"k", b".s"])),
        b"\"hello\"".to_vec()
    );
    assert_eq!(int_of(&call(&s, "json.strlen", &[b"k", b".s"])), 5);
    assert_eq!(
        call(&s, "json.strlen", &[b"k", b".zzz"]),
        b"$-1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.strappend", &[b"k", b".s", b"123"]),
        b"-ERR wrong value type: expected string\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.strappend", &[b"k", b".zzz", b"\"x\""]),
        b"-ERR path does not exist\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.strlen", &[b"k"]),
        b"-ERR wrong type of path value\r\n".to_vec()
    );
}

#[test]
fn numincrby_formats() {
    let (_g, s) = shared_for("127.0.0.1:40707");
    call(&s, "json.set", &[b"k", b".", br#"{"i":1,"f":3.0}"#]);
    assert_eq!(
        bulk_of(&call(&s, "json.numincrby", &[b"k", b".i", b"2"])),
        b"3".to_vec()
    );
    assert_eq!(
        bulk_of(&call(&s, "json.numincrby", &[b"k", b".f", b"0.5"])),
        b"3.5".to_vec()
    );
    assert_eq!(
        bulk_of(&call(&s, "json.numincrby", &[b"k", b".i", b"0.5"])),
        b"3.5".to_vec()
    );
    assert_eq!(
        call(&s, "json.numincrby", &[b"k", b".i", b"zz"]),
        b"-ERR value is not a float\r\n".to_vec()
    );
    assert_eq!(
        call(&s, "json.numincrby", &[b"k", b".zzz", b"1"]),
        b"-ERR path does not exist\r\n".to_vec()
    );
    call(&s, "json.set", &[b"s", b".", b"\"x\""]);
    assert_eq!(
        call(&s, "json.numincrby", &[b"s", b".", b"1"]),
        b"-ERR wrong type of path value\r\n".to_vec()
    );
    call(&s, "json.set", &[b"m", b".", b"1e308"]);
    assert_eq!(
        call(&s, "json.numincrby", &[b"m", b".", b"1e308"]),
        b"-ERR result is not a number or out of range\r\n".to_vec()
    );
}
