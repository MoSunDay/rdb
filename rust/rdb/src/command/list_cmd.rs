//! List commands, part 1 (LPUSH/RPUSH/LPUSHX/RPUSHX, LLEN, LRANGE,
//! LINDEX, LSET) plus the state/commit helpers shared by every list
//! handler: keys resolve via `keys_core::resolve` (lazy expiry +
//! wrong-type detection), elements read/write through `ds::list_ds`,
//! and every mutation lands in ONE batched fsync under the per-key
//! latch. Pops, LREM/LTRIM live in `list_ops`; LINSERT/LPOS/LMOVE in
//! `list_move`; the blocking variants in `list_block`.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::{keys_core, Ctx};
use crate::ds::codec::KIND_LIST_META;
use crate::ds::{expire, latch, list_ds, wait};
use crate::resp::codec::{
    append_array, append_bulk, append_error, append_int, append_null, append_string,
};
use crate::store::{ops, Store};

/// Blank meta of a list that does not exist yet (no TTL, no entries).
pub(crate) fn blank_meta() -> list_ds::ListMeta {
    list_ds::ListMeta {
        expire_ms: 0,
        l_count: 0,
        l_next: 0,
        r_count: 0,
        r_next: 0,
    }
}

/// What one key is from the list commands' point of view.
#[derive(Debug, PartialEq)]
pub(crate) enum ListState {
    Missing,
    List {
        expire_ms: u64,
        meta: list_ds::ListMeta,
    },
    WrongType,
}

/// Resolve via keys_core (raw strings and foreign kinds -> WrongType);
/// an expired list purges and reads as Missing.
pub(crate) fn list_state(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> ListState {
    match keys_core::resolve(store, prefix, key, now) {
        keys_core::KeyState::Missing => ListState::Missing,
        keys_core::KeyState::RawString { .. } => ListState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_LIST_META => {
            ListState::WrongType
        }
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => ListState::List {
            expire_ms,
            meta: list_ds::decode_meta_payload(expire_ms, &payload),
        },
    }
}

/// Commit one list mutation under the already-held latch: the caller's
/// entry puts/deletes plus the meta record -- or a full family wipe when
/// `meta` is `None` (empty lists do not exist, Redis semantics) -- in a
/// single fsync. `true` after a successful write; the error reply is
/// written here on failure. A list's TTL never changes inside a list
/// command, so `set_ttl_entries` only re-asserts the unchanged index
/// entry (mirrors `set_cmd::commit`).
pub(crate) async fn commit_list(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    meta: Option<&list_ds::ListMeta>,
    batch: WriteBatch,
    cmd: &str,
) -> bool {
    let mut batch = batch;
    match meta {
        None => list_ds::delete_family(&mut batch, &ctx.prefix_key, key, expire_ms),
        Some(m) => {
            let root = list_ds::meta_key(&ctx.prefix_key, key);
            expire::set_ttl_entries(&mut batch, &ctx.prefix_key, root, expire_ms, expire_ms);
            list_ds::write_meta(&mut batch, &ctx.prefix_key, key, m);
        }
    }
    ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .map(|_| true)
        .unwrap_or_else(|_| {
            append_error(ctx.out, &format!("ERR: {cmd} failed"));
            false
        })
}

/// Lock every DISTINCT latch key of `keys` in byte order (the multi-key
/// ABBA rule) and return the guards; they release on drop.
pub(crate) async fn lock_sorted(ctx: &Ctx<'_>, keys: &[Vec<u8>]) -> Vec<latch::KeyGuard> {
    let mut latches: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| keys_core::latch_key(&ctx.prefix_key, k))
        .collect();
    latches.sort();
    latches.dedup();
    let mut guards = Vec::with_capacity(latches.len());
    for k in &latches {
        guards.push(latch::lock(&ctx.shared.latch, k).await);
    }
    guards
}

/// Plan a single-element pop off one end WITHOUT writing: fetches the
/// target element via the O(1) pop target and returns it next to the
/// meta that holds after the removal. The caller deletes the entry and
/// commits. `Err` = store read failure.
pub(crate) fn pop_one(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    meta: &list_ds::ListMeta,
    left: bool,
) -> Result<(Vec<u8>, list_ds::ListMeta), String> {
    let (is_left, idx) = if left {
        list_ds::pop_left_target(meta)
    } else {
        list_ds::pop_right_target(meta)
    };
    let elem = if is_left {
        list_ds::get_l(store, prefix, key, idx)?
    } else {
        list_ds::get_r(store, prefix, key, idx)?
    }
    .unwrap_or_default();
    let mut after = *meta;
    // Top removals (the usual case) shrink `next` too; bottom removals
    // (the cross-side fallback) only shrink `count`, so the base moves.
    if is_left {
        after.l_count -= 1;
        if idx == meta.l_next - 1 {
            after.l_next -= 1;
        }
    } else {
        after.r_count -= 1;
        if idx == meta.r_next - 1 {
            after.r_next -= 1;
        }
    }
    Ok((elem, after))
}

/// Shared body of LPUSH/RPUSH/LPUSHX/RPUSHX: append every element at
/// one end in one batch and reply the new length. `create` = false
/// (the *X variants) leaves a missing key untouched and replies 0.
/// Pushes wake as many parked blocking readers as elements landed.
async fn push_variant(ctx: &mut Ctx<'_>, left: bool, create: bool, cmd: &str) {
    if ctx.args.len() < 2 {
        arity(ctx.out, cmd);
        return;
    }
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let mut meta = match list_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        ListState::List { meta, .. } => meta,
        ListState::Missing if create => blank_meta(),
        ListState::Missing => {
            append_int(ctx.out, 0);
            return;
        }
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let mut batch = WriteBatch::default();
    for (i, elem) in ctx.args[1..].iter().enumerate() {
        if left {
            list_ds::put_l(
                &mut batch,
                &ctx.prefix_key,
                &key,
                meta.l_next + i as u64,
                elem,
            );
        } else {
            list_ds::put_r(
                &mut batch,
                &ctx.prefix_key,
                &key,
                meta.r_next + i as u64,
                elem,
            );
        }
    }
    let n = (ctx.args.len() - 1) as u64;
    if left {
        meta.l_next += n;
        meta.l_count += n;
    } else {
        meta.r_next += n;
        meta.r_count += n;
    }
    let len = meta.len();
    if commit_list(ctx, &key, meta.expire_ms, Some(&meta), batch, cmd).await {
        wait::notify_n(
            &ctx.shared.wait_hub,
            &list_ds::meta_key(&ctx.prefix_key, &key),
            n as usize,
        );
        append_int(ctx.out, len as i64);
    }
}

/// LPUSH key element [element ...] -> new length.
pub async fn lpush(ctx: &mut Ctx<'_>) {
    push_variant(ctx, true, true, "lpush").await;
}

/// RPUSH key element [element ...] -> new length.
pub async fn rpush(ctx: &mut Ctx<'_>) {
    push_variant(ctx, false, true, "rpush").await;
}

/// LPUSHX key element [element ...] -> new length (0 when key missing).
pub async fn lpushx(ctx: &mut Ctx<'_>) {
    push_variant(ctx, true, false, "lpushx").await;
}

/// RPUSHX key element [element ...] -> new length (0 when key missing).
pub async fn rpushx(ctx: &mut Ctx<'_>) {
    push_variant(ctx, false, false, "rpushx").await;
}

/// LLEN key -> element count (0 for missing keys).
pub async fn llen(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "llen");
        return;
    }
    match list_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        ListState::List { meta, .. } => append_int(ctx.out, meta.len() as i64),
        ListState::Missing => append_int(ctx.out, 0),
        ListState::WrongType => append_error(ctx.out, WRONGTYPE),
    }
}

/// Resolve a `[start, stop]` pair with Redis clamping rules (negatives
/// count from the back, `-1` = last) into logical `Some((from, to))`;
/// `None` when the range selects no element at all.
pub(crate) fn clamp_range(start: i64, stop: i64, len: u64) -> Option<(u64, u64)> {
    let len_i = i64::try_from(len).unwrap_or(i64::MAX);
    let s = if start < 0 {
        (len_i + start).max(0)
    } else {
        start.min(len_i)
    };
    let e = if stop < 0 { len_i + stop } else { stop };
    if e < 0 || s > e || s >= len_i {
        return None;
    }
    Some((s as u64, e.min(len_i - 1) as u64))
}

/// LRANGE key start stop -> elements `[start..=stop]` in logical order.
pub async fn lrange(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "lrange");
        return;
    }
    let (Some(start), Some(stop)) = (parse_i64(&ctx.args[1]), parse_i64(&ctx.args[2])) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
    let key = ctx.args[0].clone();
    let meta = match list_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        ListState::List { meta, .. } => meta,
        ListState::Missing => {
            append_array(ctx.out, 0);
            return;
        }
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let elems = match clamp_range(start, stop, meta.len()) {
        None => Vec::new(),
        Some((from, to)) => {
            match list_ds::collect_range(&ctx.shared.store, &ctx.prefix_key, &key, &meta, from, to)
            {
                Ok(v) => v,
                Err(_) => {
                    append_error(ctx.out, "ERR: lrange failed");
                    return;
                }
            }
        }
    };
    append_array(ctx.out, elems.len());
    for e in &elems {
        append_bulk(ctx.out, e);
    }
}

/// LINDEX key index -> element at `index` (negatives from the back);
/// null bulk when out of range or the key is missing.
pub async fn lindex(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "lindex");
        return;
    }
    let Some(index) = parse_i64(&ctx.args[1]) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
    let key = ctx.args[0].clone();
    let meta = match list_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        ListState::List { meta, .. } => meta,
        ListState::Missing => {
            append_null(ctx.out);
            return;
        }
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let fetched = list_ds::position_of(&meta, index)
        .map(|p| list_ds::locate(&meta, p))
        .map(|(is_left, idx)| {
            if is_left {
                list_ds::get_l(&ctx.shared.store, &ctx.prefix_key, &key, idx)
            } else {
                list_ds::get_r(&ctx.shared.store, &ctx.prefix_key, &key, idx)
            }
        });
    match fetched {
        Some(Ok(Some(elem))) => append_bulk(ctx.out, &elem),
        Some(Ok(None)) => append_null(ctx.out), // dense window: unreachable, defensive
        Some(Err(_)) => append_error(ctx.out, "ERR: lindex failed"),
        None => append_null(ctx.out),
    }
}

/// LSET key index element -> replace in place; the length never changes
/// so no blocking reader is woken.
pub async fn lset(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "lset");
        return;
    }
    let (key, index, elem) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    let Some(index) = parse_i64(&index) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let (expire_ms, meta) =
        match list_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
            ListState::List { expire_ms, meta } => (expire_ms, meta),
            ListState::Missing => {
                append_error(ctx.out, "ERR no such key");
                return;
            }
            ListState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        };
    let Some(p) = list_ds::position_of(&meta, index) else {
        append_error(ctx.out, "ERR index out of range");
        return;
    };
    let (is_left, idx) = list_ds::locate(&meta, p);
    let mut batch = WriteBatch::default();
    if is_left {
        list_ds::put_l(&mut batch, &ctx.prefix_key, &key, idx, &elem);
    } else {
        list_ds::put_r(&mut batch, &ctx.prefix_key, &key, idx, &elem);
    }
    if commit_list(ctx, &key, expire_ms, Some(&meta), batch, "lset").await {
        append_string(ctx.out, "OK");
    }
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod list_tests;
