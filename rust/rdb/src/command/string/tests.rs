//! Basic string-command tests (connection replies, GET/SET/DEL, MGET/MSET).
use super::test_util::{call, shared_for};
use super::*;
use crate::command::test_ctx;

#[test]
fn ping_quit_config_replies() {
    let (_guard, shared) = shared_for("127.0.0.1:40101");
    assert_eq!(call(&shared, |ctx| Box::pin(ping(ctx)), &[]), b"+PONG\r\n");

    let mut out = Vec::new();
    {
        let mut ctx = test_ctx(&shared, vec![], vec![], &mut out);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(quit(&mut ctx));
        assert!(ctx.close_conn);
    }
    // BREAKING (approved): Redis replies a single +OK to QUIT.
    assert_eq!(out, b"+OK\r\n");

    // "cluster-require-full-coverage" is 29 bytes long.
    assert_eq!(
        call(&shared, |ctx| Box::pin(config(ctx)), &[b"ignored", b"args"]),
        b"*2\r\n$29\r\ncluster-require-full-coverage\r\n$2\r\nno\r\n"
    );
}

#[test]
fn set_get_del_roundtrip_with_del_bug_fix() {
    let (_guard, shared) = shared_for("127.0.0.1:40102");
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"k", b"v"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"k"]),
        b"$1\r\nv\r\n"
    );
    assert_eq!(call(&shared, |ctx| Box::pin(del(ctx)), &[b"k"]), b":1\r\n");
    assert_eq!(call(&shared, |ctx| Box::pin(get(ctx)), &[b"k"]), b"$-1\r\n");
    // Bug fix: Go always answered 1; missing keys now answer 0.
    assert_eq!(call(&shared, |ctx| Box::pin(del(ctx)), &[b"k"]), b":0\r\n");
}

#[test]
fn get_set_del_arity_errors() {
    let (_guard, shared) = shared_for("127.0.0.1:40103");
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[]),
        b"-ERR wrong number of arguments for 'get' command\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"a", b"b"]),
        b"-ERR wrong number of arguments for 'get' command\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"only-key"]),
        b"-ERR wrong number of arguments for 'set' command\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[]),
        b"-ERR wrong number of arguments for 'set' command\r\n"
    );
    // With a value present, trailing junk is a syntax error (options),
    // while `SET key` alone stays an arity error.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"k", b"v", b"EX"]),
        b"-ERR syntax error\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(del(ctx)), &[]),
        b"-ERR wrong number of arguments for del command\r\n"
    );
}

#[test]
fn mget_null_for_missing_and_empty() {
    let (_guard, shared) = shared_for("127.0.0.1:40104");
    // Same-slot keys ({t} tag) so the CROSSSLOT guard stays quiet.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"{t}a", b"1"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"{t}b", b"2"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mget(ctx)),
            &[b"{t}a", b"{t}missing", b"{t}b"]
        ),
        b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[]),
        b"-ERR wrong number of arguments for 'mget' command\r\n"
    );
}

#[test]
fn mset_even_success_odd_error_returned() {
    let (_guard, shared) = shared_for("127.0.0.1:40105");
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mset(ctx)),
            &[b"{t}a", b"1", b"{t}b", b"2"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"{t}a", b"{t}b"]),
        b"*2\r\n$1\r\n1\r\n$1\r\n2\r\n"
    );
    // Bug fix: Go fell through into a panic after writing this error;
    // Rust returns cleanly with the error as the only reply.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mset(ctx)),
            &[b"{t}a", b"1", b"dangling"]
        ),
        b"-ERR wrong number of arguments: 3\r\n"
    );
}

#[test]
fn mget_empty_string_value_is_empty_bulk_not_null() {
    let (_guard, shared) = shared_for("127.0.0.1:40115");
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"empty", b""]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"empty"]),
        b"$0\r\n\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"empty"]),
        b"*1\r\n$0\r\n\r\n"
    );
    // Missing stays null next to the empty string (same slot: {t}).
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mget(ctx)),
            &[b"{t}empty", b"{t}missing"]
        ),
        b"*2\r\n$-1\r\n$-1\r\n"
    );
    // The stored empty string also renders as a $0 bulk, not null.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"{t}empty", b""]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mget(ctx)),
            &[b"{t}empty", b"{t}missing"]
        ),
        b"*2\r\n$0\r\n\r\n$-1\r\n"
    );
}

#[test]
fn mget_wrongtype_on_non_string_key() {
    let (_guard, shared) = shared_for("127.0.0.1:40116");
    // A hash (with and without TTL) and a plain string, one slot.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(crate::command::hash_cmd::hset(ctx)),
            &[b"{t}h", b"f", b"v"]
        ),
        b":1\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(crate::command::keys::pexpire(ctx)),
            &[b"{t}h", b"60000"]
        ),
        b":1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"{t}s", b"1"]),
        b"+OK\r\n"
    );
    // Any wrong-typed key fails the WHOLE command, in any position.
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"{t}h", b"{t}s"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"{t}s", b"{t}h"]),
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
    );
    // Only strings: values come back, missing stays null.
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"{t}s", b"{t}nope"]),
        b"*2\r\n$1\r\n1\r\n$-1\r\n"
    );
}

#[test]
fn mset_zero_args_is_arity_error() {
    let (_guard, shared) = shared_for("127.0.0.1:40117");
    assert_eq!(
        call(&shared, |ctx| Box::pin(mset(ctx)), &[]),
        b"-ERR wrong number of arguments for 'mset' command\r\n"
    );
}

#[test]
fn mget_crossslot_replies_error() {
    let (_guard, shared) = shared_for("127.0.0.1:40118");
    // No hash tags: two different keys, two different slots.
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"one", b"two"]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n"
    );
    // A shared {tag} aggregates both keys into one slot: they pass.
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"{t}one", b"{t}two"]),
        b"*2\r\n$-1\r\n$-1\r\n"
    );
    // Single-key requests are always same-slot.
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"solo"]),
        b"*1\r\n$-1\r\n"
    );
}

#[test]
fn mset_crossslot_writes_nothing() {
    let (_guard, shared) = shared_for("127.0.0.1:40119");
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mset(ctx)),
            &[b"one", b"1", b"two", b"2"]
        ),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n"
    );
    // The batch never ran: nothing was created.
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"one"]),
        b"*1\r\n$-1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"two"]),
        b"*1\r\n$-1\r\n"
    );
    // Same-slot pairs go through atomically.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(mset(ctx)),
            &[b"{t}one", b"1", b"{t}two", b"2"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(mget(ctx)), &[b"{t}one", b"{t}two"]),
        b"*2\r\n$1\r\n1\r\n$1\r\n2\r\n"
    );
}
