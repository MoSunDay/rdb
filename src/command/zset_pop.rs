//! Sorted-set commands, part 3: ZREM plus ZPOPMIN/ZPOPMAX. Removals
//! delete both records of each member (lookup + ordered score) in ONE
//! batched fsync under the per-key latch; a zset drained to empty is
//! deleted (Redis semantics). No members are ever added here, so no
//! blocking reader is woken.

use std::collections::VecDeque;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::zset_util::{append_score, commit_zset, write_meta_of, zset_state, ZSetState};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, zset_ds};
use crate::resp::codec::{append_array, append_bulk, append_error, append_int};

/// ZREM key member [member ...] -> members removed; the last one
/// deletes the whole zset. Duplicate arguments count once.
pub async fn zrem(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "zrem");
        return;
    }
    let key = ctx.args[0].clone();
    let mut members: Vec<Vec<u8>> = Vec::new();
    for m in &ctx.args[1..] {
        if !members.contains(m) {
            members.push(m.clone());
        }
    }
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    if base == 0 {
        append_int(ctx.out, 0);
        return;
    }
    let mut batch = WriteBatch::default();
    let mut removed = 0u64;
    for member in &members {
        match zset_ds::member_score(&ctx.shared.store, &ctx.prefix_key, &key, member) {
            Ok(Some(old)) => {
                zset_ds::del_member(&mut batch, &ctx.prefix_key, &key, member);
                zset_ds::del_scored(&mut batch, &ctx.prefix_key, &key, old, member);
                removed += 1;
            }
            Ok(None) => {}
            Err(_) => {
                append_error(ctx.out, "ERR: zrem failed");
                return;
            }
        }
    }
    if removed == 0 {
        append_int(ctx.out, 0);
        return;
    }
    if commit_zset(ctx, &key, expire_ms, base - removed, batch, "zrem").await {
        append_int(ctx.out, removed as i64);
    }
}

/// ZPOPMIN/ZPOPMAX key [count] -> up to `count` (default 1) members
/// from the low (or high) end as a flat `[member, score, ...]` array;
/// `0` is an empty array, negatives error. One ascending scan collects
/// them: MIN takes the FIRST `count`, MAX keeps the LAST `count`
/// (bounded deque, so memory follows the reply size).
async fn pop(ctx: &mut Ctx<'_>, max: bool, cmd: &str) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, cmd);
        return;
    }
    let count: Option<i64> = match ctx.args.get(1) {
        None => None,
        Some(arg) => match parse_i64(arg) {
            Some(n) => Some(n),
            None => {
                append_error(ctx.out, "ERR value is not an integer or out of range");
                return;
            }
        },
    };
    if count.is_some_and(|c| c < 0) {
        append_error(ctx.out, "ERR value is out of range, must be positive");
        return;
    }
    if count == Some(0) {
        append_array(ctx.out, 0);
        return;
    }
    let n = count.unwrap_or(1) as u64;
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let (expire_ms, base) =
        match zset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
            ZSetState::ZSet { expire_ms, count } => (expire_ms, count),
            ZSetState::Missing => {
                append_array(ctx.out, 0);
                return;
            }
            ZSetState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        };
    if base == 0 {
        append_array(ctx.out, 0);
        return;
    }
    let mut window: VecDeque<(Vec<u8>, f64)> = VecDeque::new();
    // A failed scan aborts BEFORE any batch is built: a partial pop
    // must never mutate storage.
    if zset_ds::for_each_scored(
        &ctx.shared.store,
        &ctx.prefix_key,
        &key,
        b"",
        false,
        &mut |member, score| {
            window.push_back((member.to_vec(), score));
            if max {
                if window.len() as u64 > n {
                    window.pop_front(); // keep only the last n
                }
                true
            } else {
                (window.len() as u64) < n // stop once n are in hand
            }
        },
    )
    .is_err()
    {
        append_error(ctx.out, &format!("ERR: {cmd} failed"));
        return;
    }
    let mut batch = WriteBatch::default();
    for (member, score) in &window {
        zset_ds::del_member(&mut batch, &ctx.prefix_key, &key, member);
        zset_ds::del_scored(&mut batch, &ctx.prefix_key, &key, *score, member);
    }
    if commit_zset(ctx, &key, expire_ms, base - window.len() as u64, batch, cmd).await {
        // MIN pops lowest-first (deque order); MAX kept the last n of
        // the ascending scan, so the reply runs through it backwards.
        let ordered: Vec<&(Vec<u8>, f64)> = if max {
            window.iter().rev().collect()
        } else {
            window.iter().collect()
        };
        append_array(ctx.out, ordered.len() * 2);
        for (member, score) in ordered {
            append_bulk(ctx.out, member);
            append_score(ctx.out, *score);
        }
    }
}

/// ZPOPMIN key [count] -> lowest-scored members, flat array.
pub async fn zpopmin(ctx: &mut Ctx<'_>) {
    pop(ctx, false, "zpopmin").await;
}

/// ZPOPMAX key [count] -> highest-scored members, flat array.
pub async fn zpopmax(ctx: &mut Ctx<'_>) {
    pop(ctx, true, "zpopmax").await;
}
