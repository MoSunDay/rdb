//! String read/write commands beyond GET/SET: APPEND / STRLEN / GETSET /
//! SETNX / SETEX / PSETEX / GETDEL / SETRANGE / GETRANGE. Mutations latch
//! the user key and rewrite via `clear_key_family` + `write_string_record`
//! (family ranges and the expire index entry -- the entries
//! `keys_core::delete_records` writes); APPEND/SETRANGE keep the deadline,
//! GETSET/SETNX write none, SETEX/PSETEX install one, GETDEL and
//! empty-result SETRANGE delete the key; reads take no latch.

use rocksdb::WriteBatch;

use crate::command::hash_cmd;
use crate::command::keys_core;
use crate::command::string::{
    arity, clear_key_family, old_string_value, reply_old_or_null, write_string_record, OldValue,
};
use crate::command::Ctx;
use crate::ds::{expire, latch};
use crate::resp::codec::{append_bulk, append_error, append_int, append_null, append_string};

const NOT_INT: &str = "ERR value is not an integer or out of range";
const SETRANGE_MAX: u64 = 536_870_911; // Redis cap: 2^29 - 1 bytes

/// Reply WRONGTYPE (the key holds a non-string kind).
fn wrongtype(ctx: &mut Ctx<'_>) {
    append_error(ctx.out, hash_cmd::WRONGTYPE);
}

/// Take the per-user-key latch for the read-modify-write.
async fn lock_key(ctx: &Ctx<'_>, key: &[u8]) -> latch::KeyGuard {
    latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, key),
    )
    .await
}

/// `APPEND key value` -> new total length; missing key acts like SET, an
/// existing string keeps its deadline.
pub async fn append(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        return arity(ctx.out, "append");
    }
    let (key, extra, now) = (ctx.args[0].clone(), ctx.args[1].clone(), expire::now_ms());
    let _guard = lock_key(ctx, &key).await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let mut val = match old_string_value(&state) {
        OldValue::Missing => Vec::new(),
        OldValue::Str(v) => v,
        OldValue::WrongType => return wrongtype(ctx),
    };
    val.extend_from_slice(&extra);
    let (len, dl) = (val.len() as i64, state.expire_ms());
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &val, dl);
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, len),
        Err(_) => append_error(ctx.out, "ERR: append failed"),
    }
}

/// `STRLEN key` -> the string length, `:0` when missing.
pub async fn strlen(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        return arity(ctx.out, "strlen");
    }
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    match old_string_value(&state) {
        OldValue::Str(v) => append_int(ctx.out, v.len() as i64),
        OldValue::Missing => append_int(ctx.out, 0),
        OldValue::WrongType => append_error(ctx.out, hash_cmd::WRONGTYPE),
    }
}

/// `GETSET key value` -> previous value (null when missing); SET
/// semantics, so the old TTL does not survive.
pub async fn getset(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        return arity(ctx.out, "getset");
    }
    let (key, val, now) = (ctx.args[0].clone(), ctx.args[1].clone(), expire::now_ms());
    let _guard = lock_key(ctx, &key).await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let old = old_string_value(&state);
    if matches!(old, OldValue::WrongType) {
        return wrongtype(ctx);
    }
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &val, 0);
    match ctx.commit(batch).await {
        Ok(()) => reply_old_or_null(ctx.out, old),
        Err(_) => append_error(ctx.out, "ERR: getset failed"),
    }
}

/// `SETNX key value` -> `:1` when created, `:0` when it already existed
/// (any kind — the NX veto fires before any type check, like Redis).
pub async fn setnx(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        return arity(ctx.out, "setnx");
    }
    let (key, val, now) = (ctx.args[0].clone(), ctx.args[1].clone(), expire::now_ms());
    let _guard = lock_key(ctx, &key).await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    // Redis: the NX veto fires for ANY existing key kind (no type check).
    if state.is_present() {
        append_int(ctx.out, 0);
        return;
    }
    let mut batch = WriteBatch::default();
    write_string_record(&mut batch, &ctx.prefix_key, &key, &val, 0);
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, 1),
        Err(_) => append_error(ctx.out, "ERR: setnx failed"),
    }
}

/// Shared SETEX/PSETEX body: `unit_ms` scales the TTL argument into an
/// absolute deadline; non-positive/overflowing TTLs give the invalid-expire
/// error; the write overwrites any previous kind.
async fn setex_common(ctx: &mut Ctx<'_>, cmd: &str, unit_ms: u64) {
    if ctx.args.len() != 3 {
        return arity(ctx.out, cmd);
    }
    let Some(n) = hash_cmd::parse_i64(&ctx.args[1]) else {
        append_error(ctx.out, NOT_INT);
        return;
    };
    let Some(deadline) = u64::try_from(n)
        .ok()
        .filter(|n| *n > 0)
        .and_then(|n| n.checked_mul(unit_ms))
        .and_then(|ms| expire::now_ms().checked_add(ms))
    else {
        let msg = format!("ERR invalid expire time in '{cmd}' command");
        append_error(ctx.out, &msg);
        return;
    };
    let (key, val, now) = (ctx.args[0].clone(), ctx.args[2].clone(), expire::now_ms());
    let _guard = lock_key(ctx, &key).await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &val, deadline);
    match ctx.commit(batch).await {
        Ok(()) => append_string(ctx.out, "OK"),
        Err(_) => append_error(ctx.out, &format!("ERR: {cmd} failed")),
    }
}

/// `SETEX key seconds value` -> `+OK` (deadline now + seconds*1000).
pub async fn setex(ctx: &mut Ctx<'_>) {
    setex_common(ctx, "setex", 1_000).await;
}

/// `PSETEX key milliseconds value` -> `+OK` (deadline now + ms).
pub async fn psetex(ctx: &mut Ctx<'_>) {
    setex_common(ctx, "psetex", 1).await;
}

/// `GETDEL key` -> the value; key and TTL are then gone (missing -> null).
pub async fn getdel(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        return arity(ctx.out, "getdel");
    }
    let (key, now) = (ctx.args[0].clone(), expire::now_ms());
    let _guard = lock_key(ctx, &key).await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let old = old_string_value(&state);
    if matches!(old, OldValue::WrongType) {
        return wrongtype(ctx);
    }
    if state.is_present() {
        let mut batch = WriteBatch::default();
        clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
        match ctx.commit(batch).await {
            Ok(()) => reply_old_or_null(ctx.out, old),
            Err(_) => append_error(ctx.out, "ERR: getdel failed"),
        }
    } else {
        append_null(ctx.out);
    }
}

/// `SETRANGE key offset value` -> the new length; zero-pads up to
/// `offset`, preserving the deadline. Missing key + empty value creates
/// nothing; an empty result deletes the existing key.
pub async fn setrange(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        return arity(ctx.out, "setrange");
    }
    let offset = match hash_cmd::parse_i64(&ctx.args[1]) {
        Some(o) if o >= 0 => o as usize,
        _ => return append_error(ctx.out, NOT_INT),
    };
    let val = ctx.args[2].clone();
    if offset as u64 + val.len() as u64 > SETRANGE_MAX {
        append_error(ctx.out, "ERR offset exceeds maximum allowed limit");
        return;
    }
    let (key, now) = (ctx.args[0].clone(), expire::now_ms());
    let _guard = lock_key(ctx, &key).await;
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let mut new_val = match old_string_value(&state) {
        OldValue::Missing => Vec::new(),
        OldValue::Str(v) => v,
        OldValue::WrongType => return wrongtype(ctx),
    };
    new_val.resize(new_val.len().max(offset + val.len()), 0);
    new_val[offset..offset + val.len()].copy_from_slice(&val);
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    if new_val.is_empty() {
        if !state.is_present() {
            append_int(ctx.out, 0); // setting nothing on a missing key
            return;
        }
        match ctx.commit(batch).await {
            Ok(()) => append_int(ctx.out, 0),
            Err(_) => append_error(ctx.out, "ERR: setrange failed"),
        }
        return;
    }
    let (len, dl) = (new_val.len() as i64, state.expire_ms());
    write_string_record(&mut batch, &ctx.prefix_key, &key, &new_val, dl);
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, len),
        Err(_) => append_error(ctx.out, "ERR: setrange failed"),
    }
}

/// Resolve `[start, end]` GETRANGE-style: negatives count from the tail,
/// out-of-range bounds clamp so the slice collapses to None.
fn resolve_range(len: usize, start: i64, end: i64) -> Option<(usize, usize)> {
    let len = len as i64;
    let s = (if start < 0 { len + start } else { start }).max(0);
    let e = (if end < 0 { len + end } else { end }).min(len - 1);
    (len > 0 && s <= e).then_some((s as usize, e as usize))
}

/// `GETRANGE key start end` -> inclusive slice; missing -> empty bulk.
pub async fn getrange(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        return arity(ctx.out, "getrange");
    }
    let (Some(start), Some(end)) = (
        hash_cmd::parse_i64(&ctx.args[1]),
        hash_cmd::parse_i64(&ctx.args[2]),
    ) else {
        append_error(ctx.out, NOT_INT);
        return;
    };
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    match old_string_value(&state) {
        OldValue::Str(v) => match resolve_range(v.len(), start, end) {
            Some((s, e)) => append_bulk(ctx.out, &v[s..=e]),
            None => append_bulk(ctx.out, b""),
        },
        OldValue::Missing => append_bulk(ctx.out, b""),
        OldValue::WrongType => append_error(ctx.out, hash_cmd::WRONGTYPE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::hash_cmd::hset;
    use crate::command::keys::{pexpire, pttl, type_};
    use crate::command::string::test_util::{call, shared_for};
    use crate::command::string::{get, set};

    const WT: &[u8] = b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n";
    const NOT_INT_R: &[u8] = b"-ERR value is not an integer or out of range\r\n";
    const SETEX_R: &[u8] = b"-ERR invalid expire time in 'setex' command\r\n";
    const PSETEX_R: &[u8] = b"-ERR invalid expire time in 'psetex' command\r\n";
    const LIMIT_R: &[u8] = b"-ERR offset exceeds maximum allowed limit\r\n";

    /// `assert_call!(&s, cmd(args...) == want)`; the `==` token keeps it one line.
    macro_rules! assert_call {
        ($s:expr, $cmd:ident($($arg:expr),*) == $want:expr) => {
            assert_eq!(call($s, |c| Box::pin($cmd(c)), &[$($arg),*]), $want);
        };
    }

    fn int_of(r: &[u8]) -> i64 {
        std::str::from_utf8(&r[1..r.len() - 2])
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn append_extends_and_keeps_ttl() {
        let (_g, s) = shared_for("127.0.0.1:40311");
        assert_call!(&s, append(b"k", b"Hello") == b":5\r\n");
        assert_call!(&s, append(b"k", b" World") == b":11\r\n");
        assert_call!(&s, get(b"k") == b"$11\r\nHello World\r\n");
        assert_call!(&s, strlen(b"k") == b":11\r\n");
        assert_call!(&s, strlen(b"none") == b":0\r\n");
        assert_call!(&s, pexpire(b"k", b"60000") == b":1\r\n");
        assert_call!(&s, append(b"k", b"!") == b":12\r\n");
        assert!(int_of(&call(&s, |c| Box::pin(pttl(c)), &[b"k"])) > 0);
    }

    #[test]
    fn getset_replies_old_clears_ttl_and_setnx_vetoes() {
        let (_g, s) = shared_for("127.0.0.1:40312");
        assert_call!(&s, getset(b"k", b"new") == b"$-1\r\n");
        assert_call!(&s, get(b"k") == b"$3\r\nnew\r\n");
        assert_call!(&s, pexpire(b"k", b"60000") == b":1\r\n");
        assert_call!(&s, getset(b"k", b"x") == b"$3\r\nnew\r\n");
        assert_call!(&s, pttl(b"k") == b":-1\r\n");
        assert_call!(&s, setnx(b"n", b"1") == b":1\r\n");
        assert_call!(&s, setnx(b"n", b"2") == b":0\r\n");
        assert_call!(&s, get(b"n") == b"$1\r\n1\r\n");
    }

    #[test]
    fn setex_psetex_validate_ttl_and_overwrite_any_kind() {
        let (_g, s) = shared_for("127.0.0.1:40313");
        assert_call!(&s, setex(b"k", b"10", b"v") == b"+OK\r\n");
        assert!(int_of(&call(&s, |c| Box::pin(pttl(c)), &[b"k"])) >= 9000);
        assert_call!(&s, psetex(b"p", b"100", b"v") == b"+OK\r\n");
        assert_call!(&s, setex(b"z", b"0", b"v") == SETEX_R);
        assert_call!(&s, psetex(b"z", b"-1", b"v") == PSETEX_R);
        assert_call!(&s, setex(b"z", b"zz", b"v") == NOT_INT_R);
        assert_call!(&s, hset(b"h", b"f", b"v") == b":1\r\n");
        assert_call!(&s, setex(b"h", b"10", b"w") == b"+OK\r\n");
        assert_call!(&s, get(b"h") == b"$1\r\nw\r\n");
    }

    #[test]
    fn getdel_returns_old_value_and_removes_the_key() {
        let (_g, s) = shared_for("127.0.0.1:40314");
        assert_call!(&s, set(b"k", b"v") == b"+OK\r\n");
        assert_call!(&s, pexpire(b"k", b"60000") == b":1\r\n");
        assert_call!(&s, getdel(b"k") == b"$1\r\nv\r\n");
        assert_call!(&s, get(b"k") == b"$-1\r\n");
        assert_call!(&s, type_(b"k") == b"+none\r\n");
        assert_call!(&s, getdel(b"k") == b"$-1\r\n");
    }

    #[test]
    fn setrange_pads_overwrites_and_deletes_empty_results() {
        let (_g, s) = shared_for("127.0.0.1:40315");
        assert_call!(&s, setrange(b"k", b"5", b"abc") == b":8\r\n");
        assert_call!(&s, get(b"k") == b"$8\r\n\x00\x00\x00\x00\x00abc\r\n");
        assert_call!(&s, setrange(b"k", b"6", b"XY") == b":8\r\n");
        assert_call!(&s, get(b"k") == b"$8\r\n\x00\x00\x00\x00\x00aXY\r\n");
        // Missing key + empty value: no create.
        assert_call!(&s, setrange(b"m", b"0", b"") == b":0\r\n");
        assert_call!(&s, get(b"m") == b"$-1\r\n");
        // Empty result on an existing key: the key is deleted.
        assert_call!(&s, set(b"e", b"") == b"+OK\r\n");
        assert_call!(&s, setrange(b"e", b"0", b"") == b":0\r\n");
        assert_call!(&s, get(b"e") == b"$-1\r\n");
        // The deadline survives the rewrite; bad offsets are refused.
        assert_call!(&s, pexpire(b"k", b"60000") == b":1\r\n");
        assert_call!(&s, setrange(b"k", b"0", b"z") == b":8\r\n");
        assert!(int_of(&call(&s, |c| Box::pin(pttl(c)), &[b"k"])) > 0);
        assert_call!(&s, setrange(b"k", b"536870911", b"z") == LIMIT_R);
        assert_call!(&s, setrange(b"k", b"zz", b"z") == NOT_INT_R);
        assert_call!(&s, setrange(b"k", b"-1", b"z") == NOT_INT_R);
    }

    #[test]
    fn getrange_negatives_and_clamps() {
        let (_g, s) = shared_for("127.0.0.1:40316");
        assert_call!(&s, set(b"k", b"Hello World") == b"+OK\r\n");
        assert_call!(&s, getrange(b"k", b"0", b"4") == b"$5\r\nHello\r\n");
        assert_call!(&s, getrange(b"k", b"-5", b"-1") == b"$5\r\nWorld\r\n");
        assert_call!(&s, getrange(b"k", b"0", b"-1") == b"$11\r\nHello World\r\n");
        assert_call!(&s, getrange(b"k", b"6", b"5") == b"$0\r\n\r\n");
        assert_call!(&s, getrange(b"k", b"-100", b"-50") == b"$0\r\n\r\n");
        assert_call!(&s, getrange(b"k", b"50", b"100") == b"$0\r\n\r\n");
        assert_call!(&s, getrange(b"m", b"0", b"1") == b"$0\r\n\r\n");
        assert_call!(&s, getrange(b"k", b"x", b"1") == NOT_INT_R);
    }

    #[test]
    fn wrongtype_against_hash_for_every_command() {
        let (_g, s) = shared_for("127.0.0.1:40317");
        assert_call!(&s, hset(b"h", b"f", b"v") == b":1\r\n");
        assert_call!(&s, append(b"h", b"x") == WT);
        assert_call!(&s, strlen(b"h") == WT);
        assert_call!(&s, getset(b"h", b"x") == WT);
        // Redis parity: NX veto fires on any kind, no type check.
        assert_call!(&s, setnx(b"h", b"x") == b":0\r\n");
        assert_call!(&s, getdel(b"h") == WT);
        assert_call!(&s, setrange(b"h", b"0", b"x") == WT);
        assert_call!(&s, getrange(b"h", b"0", b"1") == WT);
        assert_call!(
            &s,
            append(b"h") == b"-ERR wrong number of arguments for 'append' command\r\n"
        );
    }
}
