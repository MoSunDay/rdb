//! Handler-level tests for `command::keys`: crate-wide store lock, fresh
//! Shared per test, current-thread tokio runtime per call.

use super::*;
use crate::command::test_ctx;
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

fn call(shared: &Shared, handler: crate::command::Handler, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = test_ctx(shared, PREFIX.to_vec(), argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(handler(&mut ctx));
    out
}

fn set_raw(shared: &Shared, key: &[u8], val: &[u8]) {
    crate::store::set(&shared.store, PREFIX, key, val).expect("raw set");
}

use crate::resp::codec::test_reader::{self, Frame};

fn int_of(reply: &[u8]) -> i64 {
    let text = String::from_utf8(reply.to_vec()).unwrap();
    text.trim_start_matches(':').trim_end().parse().unwrap()
}

#[test]
fn type_exists_and_del_roundtrip() {
    let (_g, s) = shared_for("127.0.0.1:40201");
    set_raw(&s, b"k", b"v");
    assert_eq!(
        call(&s, |c| Box::pin(type_(c)), &[b"k"]),
        b"+string\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), &[b"k"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(call(&s, |c| Box::pin(del(c)), &[b"k"]), b":1\r\n".to_vec());
    assert_eq!(
        call(&s, |c| Box::pin(type_(c)), &[b"k"]),
        b"+none\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), &[b"k"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(call(&s, |c| Box::pin(del(c)), &[b"k"]), b":0\r\n".to_vec());

    // Typed records TYPE by their kind: write a hash meta directly.
    let meta = crate::ds::codec::data_key(PREFIX, crate::ds::codec::KIND_HASH_META, b"h");
    crate::store::ops::batch_write(&s.store, {
        let mut b = rocksdb::WriteBatch::default();
        b.put(&meta, crate::ds::codec::encode_envelope(0, b"1"));
        b
    })
    .expect("batch");
    assert_eq!(
        call(&s, |c| Box::pin(type_(c)), &[b"h"]),
        b"+hash\r\n".to_vec()
    );
    // EXISTS counts multiple keys; DEL counts only real deletions.
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), &[b"h", b"nope"]),
        b":1\r\n".to_vec()
    );
    set_raw(&s, b"j", b"w");
    assert_eq!(
        call(&s, |c| Box::pin(del(c)), &[b"h", b"j", b"gone"]),
        b":2\r\n".to_vec()
    );
}

#[test]
fn expire_flag_matrix() {
    let (_g, s) = shared_for("127.0.0.1:40202");
    set_raw(&s, b"k", b"v");
    // NX: first set wins, second refused.
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"100", b"NX"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"50", b"NX"]),
        b":0\r\n".to_vec()
    );
    // GT: only strictly larger deadlines (>=1s margin beats clock jitter).
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"101", b"GT"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"500", b"GT"]),
        b":1\r\n".to_vec()
    );
    // LT: only strictly smaller.
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"500", b"LT"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"99", b"LT"]),
        b":1\r\n".to_vec()
    );
    // XX on a no-TTL key is refused; case-insensitive flags work.
    set_raw(&s, b"w", b"v");
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"w", b"100", b"xx"]),
        b":0\r\n".to_vec()
    );
    // Unknown flag and non-integer values error out.
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"100", b"ZZ"]),
        b"-ERR Unsupported option: supported options are NX, XX, GT and LT\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"k", b"abc"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
    // Missing key: 0 for every variant.
    assert_eq!(
        call(&s, |c| Box::pin(expire(c)), &[b"nope", b"100"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(pexpire(c)), &[b"nope", b"100"]),
        b":0\r\n".to_vec()
    );
}

#[test]
fn ttl_pttl_persist_flow() {
    let (_g, s) = shared_for("127.0.0.1:40203");
    set_raw(&s, b"k", b"v");
    assert_eq!(call(&s, |c| Box::pin(ttl(c)), &[b"k"]), b":-1\r\n".to_vec());
    assert_eq!(
        call(&s, |c| Box::pin(ttl(c)), &[b"gone"]),
        b":-2\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(pexpire(c)), &[b"k", b"60000"]),
        b":1\r\n".to_vec()
    );
    let ms = int_of(&call(&s, |c| Box::pin(pttl(c)), &[b"k"]));
    assert!(ms > 0 && ms <= 60_000, "pttl {ms}");
    let secs = int_of(&call(&s, |c| Box::pin(ttl(c)), &[b"k"]));
    assert!((59..=60).contains(&secs), "ttl {secs}");
    assert_eq!(
        call(&s, |c| Box::pin(persist(c)), &[b"k"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(call(&s, |c| Box::pin(ttl(c)), &[b"k"]), b":-1\r\n".to_vec());
    assert_eq!(
        call(&s, |c| Box::pin(persist(c)), &[b"k"]),
        b":0\r\n".to_vec()
    );
    // PERSIST on a never-expiring raw key and on a missing key: both 0.
    assert_eq!(
        call(&s, |c| Box::pin(persist(c)), &[b"k"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(persist(c)), &[b"gone"]),
        b":0\r\n".to_vec()
    );
}

#[test]
fn expire_in_past_deletes_and_expireat_absolute() {
    let (_g, s) = shared_for("127.0.0.1:40204");
    set_raw(&s, b"k", b"v");
    // Absolute deadline in 1970: immediately due -> key deleted.
    assert_eq!(
        call(&s, |c| Box::pin(pexpireat(c)), &[b"k", b"1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), &[b"k"]),
        b":0\r\n".to_vec()
    );
    // EXPIREAT a few seconds out reports the remaining seconds (floored).
    set_raw(&s, b"k2", b"v");
    let deadline = (crate::ds::expire::now_ms() as i64 / 1000 + 3).to_string();
    assert_eq!(
        call(&s, |c| Box::pin(expireat(c)), &[b"k2", deadline.as_bytes()]),
        b":1\r\n".to_vec()
    );
    let secs = int_of(&call(&s, |c| Box::pin(ttl(c)), &[b"k2"]));
    assert!((2..=3).contains(&secs), "ttl {secs}");
}

#[test]
fn scan_pages_match_count_and_errors() {
    let (_g, s) = shared_for("127.0.0.1:40205");
    for i in 0..5u8 {
        set_raw(&s, format!("k{i}").as_bytes(), b"v");
    }
    // Full iteration through COUNT 2 pages returns every key exactly once.
    let mut cursor = b"0".to_vec();
    let mut seen: Vec<Vec<u8>> = Vec::new();
    loop {
        let reply = call(&s, |c| Box::pin(scan(c)), &[&cursor, b"COUNT", b"2"]);
        let page = test_reader::parse(&reply);
        cursor = test_reader::bulk(&page[0]);
        let Frame::Array(keys) = &page[1] else {
            panic!("key array")
        };
        seen.extend(keys.iter().map(test_reader::bulk));
        if cursor == b"0" {
            break;
        }
    }
    seen.sort();
    let want: Vec<Vec<u8>> = (0..5).map(|i| format!("k{i}").into_bytes()).collect();
    assert_eq!(seen, want);

    // MATCH filters; empty result is still a valid page with cursor "0".
    let reply = call(&s, |c| Box::pin(scan(c)), &[b"0", b"MATCH", b"k[23]"]);
    let page = test_reader::parse(&reply);
    let keys: Vec<Vec<u8>> = match &page[1] {
        Frame::Array(ks) => ks.iter().map(test_reader::bulk).collect(),
        _ => panic!("key array"),
    };
    assert_eq!(keys, vec![b"k2".to_vec(), b"k3".to_vec()]);
    assert_eq!(
        call(&s, |c| Box::pin(scan(c)), &[b"0", b"MATCH", b"zz*"]),
        b"*2\r\n$1\r\n0\r\n*0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(scan(c)), &[b"not-hex!"]),
        b"-ERR invalid cursor\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(scan(c)), &[b"0", b"BOGUS", b"x"]),
        b"-ERR syntax error\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(scan(c)), &[b"0", b"COUNT", b"0"]),
        b"-ERR value is not an integer or out of range\r\n".to_vec()
    );
}

#[test]
fn keys_pattern_and_randomkey() {
    let (_g, s) = shared_for("127.0.0.1:40206");
    assert_eq!(
        call(&s, |c| Box::pin(keys_cmd(c)), &[b"*"]),
        b"*0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(randomkey(c)), &[]),
        b"$-1\r\n".to_vec()
    );
    set_raw(&s, b"alpha", b"1");
    set_raw(&s, b"beta", b"2");
    assert_eq!(
        call(&s, |c| Box::pin(keys_cmd(c)), &[b"a*"]),
        b"*1\r\n$5\r\nalpha\r\n".to_vec()
    );
    let rk = call(&s, |c| Box::pin(randomkey(c)), &[]);
    assert!(rk == b"$5\r\nalpha\r\n".to_vec() || rk == b"$4\r\nbeta\r\n".to_vec());
}

#[test]
fn rename_moves_raw_and_ttl_records() {
    let (_g, s) = shared_for("127.0.0.1:40207");
    set_raw(&s, b"a", b"1");
    set_raw(&s, b"b", b"2");
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), &[b"a", b"c"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), &[b"a"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(crate::command::string::get(c)), &[b"c"]),
        b"$1\r\n1\r\n".to_vec()
    );
    // Plain RENAME overwrites the destination.
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), &[b"c", b"b"]),
        b"+OK\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(crate::command::string::get(c)), &[b"b"]),
        b"$1\r\n1\r\n".to_vec()
    );
    // RENAMENX refuses when the destination exists; works when not.
    set_raw(&s, b"d", b"9");
    assert_eq!(
        call(&s, |c| Box::pin(renamenx(c)), &[b"d", b"b"]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(renamenx(c)), &[b"d", b"e"]),
        b":1\r\n".to_vec()
    );
    // Missing source, and self-rename of an existing key.
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), &[b"nope", b"x"]),
        b"-ERR no such key\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), &[b"e", b"e"]),
        b"+OK\r\n".to_vec()
    );
    // TTL travels with the renamed record.
    assert_eq!(
        call(&s, |c| Box::pin(pexpire(c)), &[b"e", b"60000"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), &[b"e", b"f"]),
        b"+OK\r\n".to_vec()
    );
    let ms = int_of(&call(&s, |c| Box::pin(pttl(c)), &[b"f"]));
    assert!(ms > 0 && ms <= 60_000, "pttl {ms}");
    assert_eq!(
        call(&s, |c| Box::pin(pttl(c)), &[b"e"]),
        b":-2\r\n".to_vec()
    );
}

#[test]
fn arity_errors_for_every_handler() {
    let (_g, s) = shared_for("127.0.0.1:40208");
    // Through the registry, like the wire path; randomkey/expire take one
    // arg so their handlers see a non-empty-but-short argv.
    let no_args: &[&[u8]] = &[];
    let one_arg: &[&[u8]] = &[b"k"];
    for (name, args) in [
        ("type", no_args),
        ("exists", no_args),
        ("del", no_args),
        ("expire", one_arg),
        ("ttl", no_args),
        ("persist", no_args),
        ("scan", no_args),
        ("keys", no_args),
        ("randomkey", one_arg),
        ("rename", one_arg),
    ] {
        let handler = crate::command::lookup(name).expect("registered");
        let reply = call(&s, handler, args);
        let expect = format!("-ERR wrong number of arguments for '{name}' command\r\n");
        assert_eq!(reply, expect.into_bytes(), "cmd {name}");
    }
}
