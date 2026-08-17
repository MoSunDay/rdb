//! Numeric hash-field commands: HINCRBY / HINCRBYFLOAT. Both take the
//! per-key latch, resolve the hash meta via `hash_cmd::write_meta_of`
//! (WRONGTYPE for foreign kinds), read the current field as an
//! integer/float and commit the new value through `hash_cmd::commit`
//! in one batched fsync.

use crate::command::hash_cmd::{arity, commit, field_exists, parse_f64, parse_i64, write_meta_of};
use crate::command::{keys_core, Ctx};
use crate::ds::{hash_ds, latch};
use crate::resp::codec::{append_bulk, append_error, append_int};

/// Read one field as i64 (missing = 0); `Err(reply)` mirrors Redis's
/// "hash value is not an integer".
fn field_as_i64(ctx: &Ctx<'_>, key: &[u8], field: &[u8]) -> Result<i64, &'static str> {
    let raw = hash_ds::read_field(&ctx.shared.store, &ctx.prefix_key, key, field);
    match raw.ok().flatten() {
        None => Ok(0),
        Some(v) if v.is_empty() => Ok(0),
        Some(v) => parse_i64(&v).ok_or("ERR hash value is not an integer"),
    }
}

/// HINCRBY key field delta -> new value.
pub async fn hincrby(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "hincrby");
        return;
    }
    let Some(delta) = parse_i64(&ctx.args[2]) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
    let (key, field) = (ctx.args[0].clone(), ctx.args[1].clone());
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    let current = match field_as_i64(ctx, &key, &field) {
        Ok(n) => n,
        Err(e) => {
            append_error(ctx.out, e);
            return;
        }
    };
    let Some(new) = current.checked_add(delta) else {
        append_error(ctx.out, "ERR increment or decrement would overflow");
        return;
    };
    let created = current == 0 && !field_exists(ctx, &key, &field);
    let put = (field, new.to_string().into_bytes());
    if commit(
        ctx,
        &key,
        expire_ms,
        base + u64::from(created),
        &[put],
        &[],
        "hincrby",
    )
    .await
    .is_ok()
    {
        append_int(ctx.out, new);
    }
}

/// HINCRBYFLOAT key field delta -> new value as a bulk string. Formatting
/// uses f64's shortest roundtrip repr (Redis prints 17 significant digits
/// of a long double; both roundtrip to the same value).
pub async fn hincrbyfloat(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "hincrbyfloat");
        return;
    }
    let Some(delta) = parse_f64(&ctx.args[2]) else {
        append_error(ctx.out, "ERR value is not a valid float");
        return;
    };
    let (key, field) = (ctx.args[0].clone(), ctx.args[1].clone());
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    let existed = field_exists(ctx, &key, &field);
    let current = match hash_ds::read_field(&ctx.shared.store, &ctx.prefix_key, &key, &field) {
        Ok(Some(v)) if !v.is_empty() => match parse_f64(&v) {
            Some(n) => n,
            None => {
                append_error(ctx.out, "ERR hash value is not a float");
                return;
            }
        },
        _ => 0.0,
    };
    let sum = current + delta;
    if !sum.is_finite() {
        append_error(ctx.out, "ERR increment would produce NaN or Infinity");
        return;
    }
    let reply = format!("{sum}").into_bytes();
    let put = (field, reply.clone());
    if commit(
        ctx,
        &key,
        expire_ms,
        base + u64::from(!existed),
        &[put],
        &[],
        "hincrbyfloat",
    )
    .await
    .is_ok()
    {
        append_bulk(ctx.out, &reply);
    }
}
