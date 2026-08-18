//! SET option-matrix tests (EX/PX/NX/XX/KEEPTTL, expiry validation).
use super::test_util::{call, shared_for};
use super::*;
use crate::state::Shared;

fn pttl_ms(shared: &Shared, key: &[u8]) -> i64 {
    let reply = call(
        shared,
        |ctx| Box::pin(crate::command::keys::pttl(ctx)),
        &[key],
    );
    let text = String::from_utf8(reply).expect("ascii");
    text.trim_start_matches(':')
        .trim_end()
        .parse()
        .expect("integer")
}

#[test]
fn set_ex_px_nx_xx_keepttl_get_full_option_matrix() {
    let (_guard, shared) = shared_for("127.0.0.1:40111");
    // EX: relative seconds -> readable now, expiring later.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v", b"EX", b"100"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"k"]),
        b"$1\r\nv\r\n"
    );
    let ms = pttl_ms(&shared, b"k");
    assert!(ms > 0 && ms <= 100_000, "ex pttl {ms}");
    // PX: relative milliseconds.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v2", b"PX", b"50000"]
        ),
        b"+OK\r\n"
    );
    let ms = pttl_ms(&shared, b"k");
    assert!(ms > 0 && ms <= 50_000, "px pttl {ms}");
    // EXAT/PXAT: absolute deadlines (seconds / milliseconds).
    let exat = (crate::ds::expire::now_ms() / 1000 + 60).to_string();
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v3", b"EXAT", exat.as_bytes()]
        ),
        b"+OK\r\n"
    );
    let ms = pttl_ms(&shared, b"k");
    assert!(ms > 0 && ms <= 60_000, "exat pttl {ms}");
    let pxat = (crate::ds::expire::now_ms() + 30_000).to_string();
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v4", b"PXAT", pxat.as_bytes()]
        ),
        b"+OK\r\n"
    );
    let ms = pttl_ms(&shared, b"k");
    assert!(ms > 0 && ms <= 30_000, "pxat pttl {ms}");
    // A past EXAT still writes -- the record is simply due at once.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"past", b"gone", b"EXAT", b"1"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"past"]),
        b"$-1\r\n"
    );
    // NX only sets absent keys; XX only existing ones.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"fresh", b"x"]),
        b"+OK\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"fresh", b"y", b"NX"]),
        b"$-1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"fresh"]),
        b"$1\r\nx\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"ghost", b"y", b"XX"]),
        b"$-1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"ghost"]),
        b"$-1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"fresh", b"z", b"XX"]),
        b"+OK\r\n"
    );
    // KEEPTTL replaces the value but carries the deadline over.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"ttl", b"a", b"PX", b"60000"]
        ),
        b"+OK\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"ttl", b"b", b"KEEPTTL"]
        ),
        b"+OK\r\n"
    );
    let ms = pttl_ms(&shared, b"ttl");
    assert!(ms > 0 && ms <= 60_000, "keepttl pttl {ms}");
    // Without KEEPTTL a plain SET drops the deadline.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"ttl", b"c"]),
        b"+OK\r\n"
    );
    assert_eq!(pttl_ms(&shared, b"ttl"), -1);
    // GET: success replies carry the OLD value instead of +OK.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"fresh", b"new", b"GET"]
        ),
        b"$1\r\nz\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"nvr", b"first", b"GET"]
        ),
        b"$-1\r\n"
    );
}

#[test]
fn set_invalid_expire_time_zero_and_negative() {
    let (_guard, shared) = shared_for("127.0.0.1:40112");
    for arg in [b"0" as &[u8], b"-5", b"-1"] {
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"k", b"v", b"EX", arg]),
            b"-ERR invalid expire time in 'set' command\r\n",
            "EX {arg:?}"
        );
    }
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v", b"PX", b"0"]
        ),
        b"-ERR invalid expire time in 'set' command\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v", b"PXAT", b"-1"]
        ),
        b"-ERR invalid expire time in 'set' command\r\n"
    );
    // Non-integer TTL arguments use the generic integer error.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"k", b"v", b"EX", b"abc"]
        ),
        b"-ERR value is not an integer or out of range\r\n"
    );
    // Nothing was written along the way.
    assert_eq!(call(&shared, |ctx| Box::pin(get(ctx)), &[b"k"]), b"$-1\r\n");
}

#[test]
fn set_keepttl_plus_ex_is_syntax_error() {
    let (_guard, shared) = shared_for("127.0.0.1:40113");
    for args in [
        vec![b"k" as &[u8], b"v", b"KEEPTTL", b"EX", b"10"],
        vec![b"k" as &[u8], b"v", b"EX", b"10", b"KEEPTTL"],
        vec![b"k" as &[u8], b"v", b"PX", b"5", b"KEEPTTL"],
        vec![b"k" as &[u8], b"v", b"KEEPTTL", b"PXAT", b"99"],
        vec![b"k" as &[u8], b"v", b"NX", b"XX"],
        vec![b"k" as &[u8], b"v", b"GET", b"GET"],
        vec![b"k" as &[u8], b"v", b"WHAT"],
    ] {
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &args),
            b"-ERR syntax error\r\n",
            "{args:?}"
        );
    }
}

#[test]
fn set_nx_veto_nil_and_get_returns_old_value_or_nil() {
    let (_guard, shared) = shared_for("127.0.0.1:40114");
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"a", b"1"]),
        b"+OK\r\n"
    );
    // NX veto: null bulk, the value survives.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"a", b"2", b"NX"]),
        b"$-1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"a"]),
        b"$1\r\n1\r\n"
    );
    // NX veto with GET: the reply is the old value, still not set.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"a", b"3", b"NX", b"GET"]
        ),
        b"$1\r\n1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"a"]),
        b"$1\r\n1\r\n"
    );
    // XX veto on a missing key: null bulk, with or without GET.
    assert_eq!(
        call(&shared, |ctx| Box::pin(set(ctx)), &[b"ghost", b"9", b"XX"]),
        b"$-1\r\n"
    );
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"ghost", b"9", b"XX", b"GET"]
        ),
        b"$-1\r\n"
    );
    // NX pass with GET: the key IS written, the old absence is nil.
    assert_eq!(
        call(
            &shared,
            |ctx| Box::pin(set(ctx)),
            &[b"ghost", b"5", b"NX", b"GET"]
        ),
        b"$-1\r\n"
    );
    assert_eq!(
        call(&shared, |ctx| Box::pin(get(ctx)), &[b"ghost"]),
        b"$1\r\n5\r\n"
    );
}
