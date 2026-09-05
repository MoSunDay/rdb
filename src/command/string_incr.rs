//! Counter string commands: INCR / DECR / INCRBY / DECRBY / INCRBYFLOAT.
//!
//! One shared shape: parse the delta first (Redis replies the parse
//! error before any key access), take the per-key latch, resolve the
//! current value as a string (missing = 0), compute and commit through
//! one batched fsync. The write PRESERVES the key's existing deadline
//! (Redis keeps TTLs across counters) by rewriting the record with
//! `state.expire_ms()`, so a STRING_TTL envelope stays enveloped and a
//! raw record stays raw.

use rocksdb::WriteBatch;

use crate::command::hash_cmd;
use crate::command::keys_core;
use crate::command::string::OldValue;
use crate::command::string::{arity, clear_key_family, old_string_value, write_string_record};
use crate::command::Ctx;
use crate::ds::{expire, latch};
use crate::resp::codec::{append_bulk, append_error, append_int};

const NOT_INT: &str = "ERR value is not an integer or out of range";
const NOT_FLOAT: &str = "ERR value is not a valid float";
const OVERFLOW: &str = "ERR increment or decrement would overflow";
const NOT_FINITE: &str = "ERR increment would produce NaN or Infinity";

/// `INCR key` -> new value (`key + 1`).
pub async fn incr(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "incr");
        return;
    }
    add_int(ctx, 1).await;
}

/// `DECR key` -> new value (`key - 1`).
pub async fn decr(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "decr");
        return;
    }
    add_int(ctx, -1).await;
}

/// `INCRBY key delta` -> new value.
pub async fn incrby(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "incrby");
        return;
    }
    let Some(delta) = hash_cmd::parse_i64(&ctx.args[1]) else {
        append_error(ctx.out, NOT_INT);
        return;
    };
    add_int(ctx, delta).await;
}

/// `DECRBY key delta` -> new value (`key - delta`); negating i64::MIN
/// overflows just like the addition would.
pub async fn decrby(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "decrby");
        return;
    }
    let Some(parsed) = hash_cmd::parse_i64(&ctx.args[1]) else {
        append_error(ctx.out, NOT_INT);
        return;
    };
    let Some(delta) = parsed.checked_neg() else {
        append_error(ctx.out, OVERFLOW);
        return;
    };
    add_int(ctx, delta).await;
}

/// Shared integer body: latch, resolve, add, commit; the reply is the
/// new value, the write keeps the key's deadline.
async fn add_int(ctx: &mut Ctx<'_>, delta: i64) {
    let key = ctx.args[0].clone();
    let now = expire::now_ms();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let current = match old_string_value(&state) {
        OldValue::Missing => 0,
        OldValue::Str(v) => match hash_cmd::parse_i64(&v) {
            Some(n) => n,
            None => {
                append_error(ctx.out, NOT_INT);
                return;
            }
        },
        OldValue::WrongType => {
            append_error(ctx.out, hash_cmd::WRONGTYPE);
            return;
        }
    };
    let Some(new) = current.checked_add(delta) else {
        append_error(ctx.out, OVERFLOW);
        return;
    };
    let val = new.to_string().into_bytes();
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &val, state.expire_ms());
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, new),
        Err(_) => append_error(ctx.out, "ERR: incr failed"),
    }
}

/// `INCRBYFLOAT key delta` -> new value as a bulk string, formatted with
/// f64's shortest roundtrip repr (mirrors `hash_incr::hincrbyfloat`).
pub async fn incrbyfloat(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "incrbyfloat");
        return;
    }
    let Some(delta) = hash_cmd::parse_f64(&ctx.args[1]) else {
        append_error(ctx.out, NOT_FLOAT);
        return;
    };
    let key = ctx.args[0].clone();
    let now = expire::now_ms();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let current = match old_string_value(&state) {
        OldValue::Missing => 0.0,
        OldValue::Str(v) => match hash_cmd::parse_f64(&v) {
            Some(n) => n,
            None => {
                append_error(ctx.out, NOT_FLOAT);
                return;
            }
        },
        OldValue::WrongType => {
            append_error(ctx.out, hash_cmd::WRONGTYPE);
            return;
        }
    };
    let sum = current + delta;
    if !sum.is_finite() {
        append_error(ctx.out, NOT_FINITE);
        return;
    }
    let reply = format!("{sum}").into_bytes();
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &reply, state.expire_ms());
    match ctx.commit(batch).await {
        Ok(()) => append_bulk(ctx.out, &reply),
        Err(_) => append_error(ctx.out, "ERR: incrbyfloat failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::hash_cmd::hset;
    use crate::command::keys::{pexpire, pttl};
    use crate::command::string::test_util::{call, shared_for};
    use crate::command::string::{get, set};

    /// Parse a lone `:n` reply.
    fn int_of(reply: &[u8]) -> i64 {
        std::str::from_utf8(&reply[1..reply.len() - 2])
            .expect("int reply")
            .parse()
            .expect("int digits")
    }

    const WRONGTYPE: &[u8] =
        b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";

    #[test]
    fn missing_key_math_and_arity() {
        let (_guard, shared) = shared_for("127.0.0.1:40301");
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}a"]),
            b":1\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(decr(ctx)), &[b"{c}b"]),
            b":-1\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrby(ctx)), &[b"{c}c", b"5"]),
            b":5\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(decrby(ctx)), &[b"{c}d", b"3"]),
            b":-3\r\n"
        );
        // Existing values add from there.
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"{c}e", b"10"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}e"]),
            b":11\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(decrby(ctx)), &[b"{c}e", b"4"]),
            b":7\r\n"
        );
        // Arity texts quote the command name.
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}a", b"x"]),
            b"-ERR wrong number of arguments for 'incr' command\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrbyfloat(ctx)), &[b"{c}a"]),
            b"-ERR wrong number of arguments for 'incrbyfloat' command\r\n"
        );
    }

    #[test]
    fn incr_family_preserves_ttl() {
        let (_guard, shared) = shared_for("127.0.0.1:40302");
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"{c}k", b"10"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(pexpire(ctx)), &[b"{c}k", b"60000"]),
            b":1\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}k"]),
            b":11\r\n"
        );
        assert!(int_of(&call(&shared, |ctx| Box::pin(pttl(ctx)), &[b"{c}k"])) > 0);
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(incrbyfloat(ctx)),
                &[b"{c}k", b"0.5"]
            ),
            b"$4\r\n11.5\r\n"
        );
        assert!(int_of(&call(&shared, |ctx| Box::pin(pttl(ctx)), &[b"{c}k"])) > 0);
        assert_eq!(
            call(&shared, |ctx| Box::pin(get(ctx)), &[b"{c}k"]),
            b"$4\r\n11.5\r\n"
        );
    }

    #[test]
    fn incr_non_integer_and_overflow_errors() {
        let (_guard, shared) = shared_for("127.0.0.1:40303");
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"{c}k", b"abc"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}k"]),
            b"-ERR value is not an integer or out of range\r\n"
        );
        // Failed call left the value untouched.
        assert_eq!(
            call(&shared, |ctx| Box::pin(get(ctx)), &[b"{c}k"]),
            b"$3\r\nabc\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrby(ctx)), &[b"{c}k", b"zz"]),
            b"-ERR value is not an integer or out of range\r\n"
        );
        // i64 saturation: INCR at the top, DECRBY of i64::MIN both spill.
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(set(ctx)),
                &[b"{c}m", b"9223372036854775807"]
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}m"]),
            b"-ERR increment or decrement would overflow\r\n"
        );
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(incrby(ctx)),
                &[b"{c}m", b"9223372036854775807"]
            ),
            b"-ERR increment or decrement would overflow\r\n"
        );
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(decrby(ctx)),
                &[b"{c}m", b"-9223372036854775808"]
            ),
            b"-ERR increment or decrement would overflow\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(decr(ctx)), &[b"{c}m"]),
            b":9223372036854775806\r\n"
        );
    }

    #[test]
    fn incr_wrongtype_against_hash() {
        let (_guard, shared) = shared_for("127.0.0.1:40304");
        assert_eq!(
            call(&shared, |ctx| Box::pin(hset(ctx)), &[b"{c}h", b"f", b"v"]),
            b":1\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incr(ctx)), &[b"{c}h"]),
            WRONGTYPE
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrby(ctx)), &[b"{c}h", b"1"]),
            WRONGTYPE
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrbyfloat(ctx)), &[b"{c}h", b"1"]),
            WRONGTYPE
        );
    }

    #[test]
    fn incrbyfloat_math_and_errors() {
        let (_guard, shared) = shared_for("127.0.0.1:40305");
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(incrbyfloat(ctx)),
                &[b"{c}f", b"3.5"]
            ),
            b"$3\r\n3.5\r\n"
        );
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(incrbyfloat(ctx)),
                &[b"{c}f", b"0.25"]
            ),
            b"$4\r\n3.75\r\n"
        );
        // Bad delta / bad current value share the "not a valid float" text.
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrbyfloat(ctx)), &[b"{c}f", b"zz"]),
            b"-ERR value is not a valid float\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"{c}s", b"nope"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(incrbyfloat(ctx)), &[b"{c}s", b"1"]),
            b"-ERR value is not a valid float\r\n"
        );
        // Non-finite sums (huge deltas or current values) are refused.
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(incrbyfloat(ctx)),
                &[b"{c}f", b"1e309"]
            ),
            b"-ERR increment would produce NaN or Infinity\r\n"
        );
        assert_eq!(
            call(&shared, |ctx| Box::pin(set(ctx)), &[b"{c}big", b"1e308"]),
            b"+OK\r\n"
        );
        assert_eq!(
            call(
                &shared,
                |ctx| Box::pin(incrbyfloat(ctx)),
                &[b"{c}big", b"1e308"]
            ),
            b"-ERR increment would produce NaN or Infinity\r\n"
        );
    }
}
