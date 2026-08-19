//! Hash commands (HSET/HSETNX/HGET/HMGET/HDEL/HLEN/HEXISTS/HSTRLEN/
//! HINCRBY/HINCRBYFLOAT): handlers resolve the key via `keys_core::resolve`
//! (lazy expiry + wrong-type detection), read/write fields through
//! `ds::hash_ds` and land every mutation in ONE batched fsync under the
//! per-key latch. Whole-hash reads (HGETALL/HKEYS/HVALS), HSCAN and
//! HRANDFIELD live in `hash_scan`.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::{keys_core, Ctx};
use crate::ds::codec::{self, KIND_HASH_META};
use crate::ds::{expire, hash_ds, latch};
use crate::resp::codec::{append_array, append_bulk, append_error, append_int, append_null};
use crate::store::ops;

pub(crate) const WRONGTYPE: &str =
    "WRONGTYPE Operation against a key holding the wrong kind of value";

pub(crate) fn arity(out: &mut Vec<u8>, cmd: &str) {
    append_error(
        out,
        &format!("ERR wrong number of arguments for '{cmd}' command"),
    );
}

pub(crate) fn parse_i64(arg: &[u8]) -> Option<i64> {
    std::str::from_utf8(arg).ok()?.parse().ok()
}

pub(crate) fn parse_f64(arg: &[u8]) -> Option<f64> {
    std::str::from_utf8(arg).ok()?.parse().ok()
}

/// What one key is from the hash commands' point of view.
#[derive(Debug, PartialEq)]
pub(crate) enum HashState {
    Missing,
    Hash { expire_ms: u64, count: u64 },
    WrongType,
}

/// Resolve via keys_core (raw strings and foreign kinds -> WrongType);
/// an expired hash purges and reads as Missing.
pub(crate) fn hash_state(ctx: &Ctx<'_>, key: &[u8]) -> HashState {
    match keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        keys_core::KeyState::Missing => HashState::Missing,
        keys_core::KeyState::RawString { .. } => HashState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_HASH_META => {
            HashState::WrongType
        }
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => HashState::Hash {
            expire_ms,
            count: codec::decode_count(&payload),
        },
    }
}

/// Meta for a write path: `(expire_ms, count)`, replying WRONGTYPE and
/// answering `None` when the key holds another type.
pub(crate) fn write_meta_of(ctx: &mut Ctx<'_>, key: &[u8]) -> Option<(u64, u64)> {
    match hash_state(ctx, key) {
        HashState::Hash { expire_ms, count } => Some((expire_ms, count)),
        HashState::Missing => Some((0, 0)),
        HashState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            None
        }
    }
}

/// Field presence; `Err` propagates the storage read failure so write
/// paths can abort instead of mistaking it for "field absent".
pub(crate) fn field_exists(ctx: &Ctx<'_>, key: &[u8], field: &[u8]) -> Result<bool, String> {
    hash_ds::read_field(&ctx.shared.store, &ctx.prefix_key, key, field).map(|v| v.is_some())
}

/// Commit one mutation: field puts/deletes plus the meta record (or a
/// full family wipe when the count hits zero), single fsync. `Ok(())`
/// after a successful write; the error reply is written here on failure.
pub(crate) async fn commit(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    count: u64,
    puts: &[(Vec<u8>, Vec<u8>)],
    deletes: &[Vec<u8>],
    cmd: &str,
) -> Result<(), ()> {
    let mut batch = WriteBatch::default();
    for (f, v) in puts {
        batch.put(hash_ds::field_key(&ctx.prefix_key, key, f), v);
    }
    for f in deletes {
        batch.delete(hash_ds::field_key(&ctx.prefix_key, key, f));
    }
    if count == 0 {
        // Last field removed: empty hashes do not exist (Redis semantics).
        hash_ds::delete_family(&mut batch, &ctx.prefix_key, key, expire_ms);
    } else {
        hash_ds::write_meta(&mut batch, &ctx.prefix_key, key, expire_ms, count);
    }
    ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .map_err(|_| append_error(ctx.out, &format!("ERR: {cmd} failed")))
}

/// HSET key field value [field value ...] -> count of NEW fields.
pub async fn hset(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 || ctx.args.len().is_multiple_of(2) {
        arity(ctx.out, "hset");
        return;
    }
    let key = ctx.args[0].clone();
    let puts: Vec<(Vec<u8>, Vec<u8>)> = ctx.args[1..]
        .chunks(2)
        .map(|c| (c[0].clone(), c[1].clone()))
        .collect();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    let mut fresh: Vec<Vec<u8>> = Vec::new(); // fields created by THIS call
    for (f, _) in &puts {
        let present = match field_exists(ctx, &key, f) {
            Ok(p) => p,
            Err(_) => {
                append_error(ctx.out, "ERR: hset failed");
                return;
            }
        };
        if !fresh.contains(f) && !present {
            fresh.push(f.clone());
        }
    }
    let count = base + fresh.len() as u64;
    if commit(ctx, &key, expire_ms, count, &puts, &[], "hset")
        .await
        .is_ok()
    {
        append_int(ctx.out, fresh.len() as i64);
    }
}

/// HSETNX key field value -> 1 when set, 0 when the field already existed.
pub async fn hsetnx(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "hsetnx");
        return;
    }
    let (key, field, value) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    match field_exists(ctx, &key, &field) {
        Ok(true) => {
            append_int(ctx.out, 0);
            return;
        }
        Ok(false) => {}
        Err(_) => {
            append_error(ctx.out, "ERR: hsetnx failed");
            return;
        }
    }
    let put = (field, value);
    if commit(ctx, &key, expire_ms, base + 1, &[put], &[], "hsetnx")
        .await
        .is_ok()
    {
        append_int(ctx.out, 1);
    }
}

/// HGET key field -> bulk value or null.
pub async fn hget(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "hget");
        return;
    }
    match hash_state(ctx, &ctx.args[0]) {
        HashState::WrongType => append_error(ctx.out, WRONGTYPE),
        HashState::Missing => append_null(ctx.out),
        HashState::Hash { .. } => {
            let v = hash_ds::read_field(
                &ctx.shared.store,
                &ctx.prefix_key,
                &ctx.args[0],
                &ctx.args[1],
            )
            .ok()
            .flatten();
            match v {
                Some(v) => append_bulk(ctx.out, &v),
                None => append_null(ctx.out),
            }
        }
    }
}

/// HMGET key field [field ...] -> array of values (nulls where absent).
pub async fn hmget(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "hmget");
        return;
    }
    if let HashState::WrongType = hash_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    append_array(ctx.out, ctx.args.len() - 1);
    for field in &ctx.args[1..] {
        match hash_ds::read_field(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], field) {
            Ok(Some(v)) => append_bulk(ctx.out, &v),
            _ => append_null(ctx.out),
        }
    }
}

/// HDEL key field [field ...] -> count removed; removing the LAST field
/// deletes the whole hash.
pub async fn hdel(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "hdel");
        return;
    }
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    if base == 0 {
        append_int(ctx.out, 0); // missing key: nothing to remove
        return;
    }
    let mut gone: Vec<Vec<u8>> = Vec::new();
    for field in &ctx.args[1..] {
        let present = match field_exists(ctx, &key, field) {
            Ok(p) => p,
            Err(_) => {
                append_error(ctx.out, "ERR: hdel failed");
                return;
            }
        };
        if !gone.contains(field) && present {
            gone.push(field.clone());
        }
    }
    let remaining = base.saturating_sub(gone.len() as u64);
    if commit(ctx, &key, expire_ms, remaining, &[], &gone, "hdel")
        .await
        .is_ok()
    {
        append_int(ctx.out, gone.len() as i64);
    }
}

/// HLEN key -> field count (0 for missing keys).
pub async fn hlen(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "hlen");
        return;
    }
    match hash_state(ctx, &ctx.args[0]) {
        HashState::Hash { count, .. } => append_int(ctx.out, count as i64),
        HashState::Missing => append_int(ctx.out, 0),
        HashState::WrongType => append_error(ctx.out, WRONGTYPE),
    }
}

/// HEXISTS key field -> 0/1.
pub async fn hexists(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "hexists");
        return;
    }
    let answer = match hash_state(ctx, &ctx.args[0]) {
        HashState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        HashState::Missing => false,
        HashState::Hash { .. } => match field_exists(ctx, &ctx.args[0], &ctx.args[1]) {
            Ok(p) => p,
            Err(_) => {
                append_error(ctx.out, "ERR: hexists failed");
                return;
            }
        },
    };
    append_int(ctx.out, i64::from(answer));
}

/// HSTRLEN key field -> value byte length (0 when absent).
pub async fn hstrlen(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "hstrlen");
        return;
    }
    let len = match hash_state(ctx, &ctx.args[0]) {
        HashState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        HashState::Missing => 0,
        HashState::Hash { .. } => match hash_ds::read_field(
            &ctx.shared.store,
            &ctx.prefix_key,
            &ctx.args[0],
            &ctx.args[1],
        ) {
            Ok(Some(v)) => v.len(),
            Ok(None) => 0,
            Err(_) => {
                append_error(ctx.out, "ERR: hstrlen failed");
                return;
            }
        },
    };
    append_int(ctx.out, len as i64);
}
