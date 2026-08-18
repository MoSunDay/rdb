//! Blocking list commands (BLPOP/BRPOP/BLMOVE/BRPOPLPUSH): the timeout
//! parks on the shared WaitHub via `spawn_blocking` (a sync Condvar);
//! push-style list commands notify a list's meta root after committing.
//! The loop mirrors `lite::read::wait_entries`: register FIRST, then one
//! latched quick-path pass over the keys in order, then park -- so a
//! notify that lands between the read and the park is never lost.
//!
//! BLMOVE/BRPOPLPUSH commit TWICE: the blocked pop lands first (the
//! element must not be lost while pushing), then the push onto dst. A
//! crash in between drops only the move's destination half. dst is
//! prechecked for wrong-type BEFORE the pop, and if it turns wrong-type
//! while the pop half is blocked, the popped element is restored to src
//! before the error reply -- an element is never silently dropped.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_f64, WRONGTYPE};
use crate::command::list_cmd::{
    blank_meta, commit_list, list_state, lock_sorted, pop_one, ListState,
};
use crate::command::list_move::parse_dirs;
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, list_ds, setops, wait};
use crate::resp::codec::{append_bulk, append_error, append_null, append_raw};

/// BLOCK 0 means "forever": park in 24h slices so Condvar math stays sane.
/// Shared with the blocking zset pops (`zset_block`).
pub(crate) const MAX_SLICE_MS: u64 = 86_400_000;

/// Outcome of a blocking pop attempt.
pub(crate) enum BlockResult {
    /// One element was popped off `key` (already committed).
    Got { key: Vec<u8>, elem: Vec<u8> },
    /// The deadline elapsed.
    Timeout,
    /// A key holds a non-list value; the caller replies WRONGTYPE.
    WrongType,
    /// The commit failed and the error reply is already in `out`.
    Failed,
}

/// Parse the trailing timeout (seconds, fractions allowed): non-finite
/// or negative values are rejected; the millisecond value saturates.
/// Shared by every blocking command (list + zset pops).
pub(crate) fn parse_timeout(arg: &[u8]) -> Option<u64> {
    let secs = parse_f64(arg)?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some((secs * 1000.0) as u64) // float -> int casts saturate
}

/// Core of BLPOP/BRPOP: cycle register -> latched quick path -> park.
/// Keys are visited in argument order; the first non-empty one wins and
/// yields exactly one element.
async fn block_pop(
    ctx: &mut Ctx<'_>,
    keys: &[Vec<u8>],
    left: bool,
    cmd: &str,
    block_ms: u64,
) -> BlockResult {
    // BLOCK 0 means "forever" (`None`): the park runs in MAX_SLICE_MS
    // slices renewed each round. Any other timeout is a hard deadline.
    let deadline = (block_ms > 0).then(|| Instant::now() + Duration::from_millis(block_ms));
    loop {
        // 1. one shared waiter under every key's root, BEFORE the read.
        let waiter = Arc::new(wait::new_waiter());
        let roots: Vec<Vec<u8>> = keys
            .iter()
            .map(|k| list_ds::meta_key(&ctx.prefix_key, k))
            .collect();
        for root in &roots {
            wait::register_shared(&ctx.shared.wait_hub, root, &waiter);
        }
        // 2. latched quick path: pop from the first non-empty key.
        for key in keys {
            let _guard = latch::lock(
                &ctx.shared.latch,
                &keys_core::latch_key(&ctx.prefix_key, key),
            )
            .await;
            match list_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
                ListState::WrongType => {
                    for root in &roots {
                        wait::unregister(&ctx.shared.wait_hub, root, &waiter);
                    }
                    return BlockResult::WrongType;
                }
                ListState::List { expire_ms, meta } if !meta.is_empty() => {
                    let planned = pop_one(&ctx.shared.store, &ctx.prefix_key, key, &meta, left)
                        .ok()
                        .map(|(elem, after)| {
                            let (is_left, idx) = if left {
                                list_ds::pop_left_target(&meta)
                            } else {
                                list_ds::pop_right_target(&meta)
                            };
                            let mut batch = WriteBatch::default();
                            if is_left {
                                list_ds::del_l(&mut batch, &ctx.prefix_key, key, idx);
                            } else {
                                list_ds::del_r(&mut batch, &ctx.prefix_key, key, idx);
                            }
                            (elem, after, batch)
                        });
                    let Some((elem, after, batch)) = planned else {
                        // store read failed: bail out like a timeout
                        for root in &roots {
                            wait::unregister(&ctx.shared.wait_hub, root, &waiter);
                        }
                        return BlockResult::Timeout;
                    };
                    let emptied = after.is_empty();
                    let committed = commit_list(
                        ctx,
                        key,
                        expire_ms,
                        if emptied { None } else { Some(&after) },
                        batch,
                        cmd,
                    )
                    .await;
                    for root in &roots {
                        wait::unregister(&ctx.shared.wait_hub, root, &waiter);
                    }
                    return if committed {
                        BlockResult::Got {
                            key: key.clone(),
                            elem,
                        }
                    } else {
                        BlockResult::Failed
                    };
                }
                ListState::List { .. } | ListState::Missing => {}
            }
        }
        // 3. deadline check, then a bounded park on the waiter. `None`
        // (BLOCK 0) never elapses: a slice expiry just renews the loop.
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            for root in &roots {
                wait::unregister(&ctx.shared.wait_hub, root, &waiter);
            }
            return BlockResult::Timeout;
        }
        let left_ms = deadline
            .map_or(MAX_SLICE_MS, |d| {
                d.saturating_duration_since(now).as_millis() as u64
            })
            .min(MAX_SLICE_MS);
        let parked = Arc::clone(&waiter);
        let woke = tokio::task::spawn_blocking(move || {
            wait::wait(&parked, Duration::from_millis(left_ms))
        })
        .await;
        for root in &roots {
            wait::unregister(&ctx.shared.wait_hub, root, &waiter);
        }
        match woke.unwrap_or(wait::WaitOutcome::Timeout) {
            wait::WaitOutcome::Timeout if deadline.is_some_and(|d| Instant::now() >= d) => {
                return BlockResult::Timeout
            }
            _ => {} // signaled, spurious, or a renewed forever-slice: loop and re-read
        }
    }
}

/// Shared body of BLPOP/BRPOP: `*2` (key, element) on success, a null
/// array on timeout.
async fn block_pop_cmd(ctx: &mut Ctx<'_>, left: bool, cmd: &str) {
    if ctx.args.len() < 2 {
        arity(ctx.out, cmd);
        return;
    }
    let Some(block_ms) = parse_timeout(&ctx.args[ctx.args.len() - 1]) else {
        append_error(ctx.out, "ERR timeout is not a float or out of range");
        return;
    };
    let keys = ctx.args[..ctx.args.len() - 1].to_vec();
    if !setops::same_slot(&keys) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    match block_pop(ctx, &keys, left, cmd, block_ms).await {
        BlockResult::Got { key, elem } => {
            append_raw(ctx.out, b"*2\r\n");
            append_bulk(ctx.out, &key);
            append_bulk(ctx.out, &elem);
        }
        BlockResult::Timeout => append_raw(ctx.out, b"*-1\r\n"),
        BlockResult::WrongType => append_error(ctx.out, WRONGTYPE),
        BlockResult::Failed => {}
    }
}

/// BLPOP key [key ...] timeout.
pub async fn blpop(ctx: &mut Ctx<'_>) {
    block_pop_cmd(ctx, true, "blpop").await;
}

/// BRPOP key [key ...] timeout.
pub async fn brpop(ctx: &mut Ctx<'_>) {
    block_pop_cmd(ctx, false, "brpop").await;
}

/// What a push onto `key` starts from: `(expire_ms, meta)`, or `None`
/// when the key holds a non-list value (the caller replies WRONGTYPE).
fn push_target(ctx: &Ctx<'_>, key: &[u8]) -> Option<(u64, list_ds::ListMeta)> {
    match list_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        ListState::List { expire_ms, meta } => Some((expire_ms, meta)),
        ListState::Missing => Some((0, blank_meta())),
        ListState::WrongType => None,
    }
}

/// Race guard for BLMOVE/BRPOPLPUSH: dst turned wrong-type while the pop
/// half was blocked, so push the popped element back onto the SAME end
/// of `key` it came from (`left`), commit, wake `key`'s blocked waiters,
/// and only then reply WRONGTYPE -- the element must never be lost. A
/// pop/push pair through `next` round-trips the physical index, so
/// order is preserved. The caller holds the src+dst latches; a failed
/// restore keeps the store-failure reply `commit_list` already wrote.
async fn restore_popped(ctx: &mut Ctx<'_>, key: &[u8], elem: &[u8], left: bool) {
    let (expire_ms, mut after) = match list_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        key,
        expire::now_ms(),
    ) {
        ListState::List { expire_ms, meta } => (expire_ms, meta),
        ListState::Missing => (0, blank_meta()),
        // src itself changed type mid-move: no list to restore onto.
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let mut batch = WriteBatch::default();
    if left {
        list_ds::put_l(&mut batch, &ctx.prefix_key, key, after.l_next, elem);
        after.l_next += 1;
        after.l_count += 1;
    } else {
        list_ds::put_r(&mut batch, &ctx.prefix_key, key, after.r_next, elem);
        after.r_next += 1;
        after.r_count += 1;
    }
    if commit_list(ctx, key, expire_ms, Some(&after), batch, "blmove").await {
        wait::notify(
            &ctx.shared.wait_hub,
            &list_ds::meta_key(&ctx.prefix_key, key),
        );
        append_error(ctx.out, WRONGTYPE);
    }
}

/// Shared body of BLMOVE/BRPOPLPUSH: block on `src` alone, then push
/// the popped element onto `dst` in a second commit under both latches.
async fn block_move(
    ctx: &mut Ctx<'_>,
    src: Vec<u8>,
    dst: Vec<u8>,
    pop_left: bool,
    push_left: bool,
    block_ms: u64,
) {
    if !setops::same_slot(&[src.clone(), dst.clone()]) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    // Precheck dst under its latch BEFORE the pop: a wrong-type dst can
    // never receive the element, so fail fast while src is still intact.
    {
        let _guard = latch::lock(
            &ctx.shared.latch,
            &keys_core::latch_key(&ctx.prefix_key, &dst),
        )
        .await;
        if push_target(ctx, &dst).is_none() {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    }
    match block_pop(
        ctx,
        std::slice::from_ref(&src),
        pop_left,
        "blmove",
        block_ms,
    )
    .await
    {
        BlockResult::Got { elem, .. } => {
            let _guards = lock_sorted(ctx, &[src.clone(), dst.clone()]).await;
            let Some((dst_expire, mut dst_after)) = push_target(ctx, &dst) else {
                // dst changed to a non-list while we were blocked: put
                // the element back onto src, then surface the error.
                restore_popped(ctx, &src, &elem, pop_left).await;
                return;
            };
            let mut batch = WriteBatch::default();
            if push_left {
                list_ds::put_l(&mut batch, &ctx.prefix_key, &dst, dst_after.l_next, &elem);
                dst_after.l_next += 1;
                dst_after.l_count += 1;
            } else {
                list_ds::put_r(&mut batch, &ctx.prefix_key, &dst, dst_after.r_next, &elem);
                dst_after.r_next += 1;
                dst_after.r_count += 1;
            }
            if commit_list(ctx, &dst, dst_expire, Some(&dst_after), batch, "blmove").await {
                wait::notify(
                    &ctx.shared.wait_hub,
                    &list_ds::meta_key(&ctx.prefix_key, &dst),
                );
                wait::notify(
                    &ctx.shared.wait_hub,
                    &list_ds::meta_key(&ctx.prefix_key, &src),
                );
                append_bulk(ctx.out, &elem);
            }
        }
        BlockResult::Timeout => append_null(ctx.out),
        BlockResult::WrongType => append_error(ctx.out, WRONGTYPE),
        BlockResult::Failed => {}
    }
}

/// BLMOVE src dst LEFT|RIGHT LEFT|RIGHT timeout. (Five ctx.args:
/// Redis's arity 6 counts the command name.)
pub async fn blmove(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 5 {
        arity(ctx.out, "blmove");
        return;
    }
    let Some((pop_left, push_left)) = parse_dirs(&ctx.args[2], &ctx.args[3]) else {
        append_error(ctx.out, "ERR syntax error");
        return;
    };
    let Some(block_ms) = parse_timeout(&ctx.args[4]) else {
        append_error(ctx.out, "ERR timeout is not a float or out of range");
        return;
    };
    block_move(
        ctx,
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        pop_left,
        push_left,
        block_ms,
    )
    .await;
}

/// BRPOPLPUSH src dst timeout = BLMOVE src dst RIGHT LEFT.
pub async fn brpoplpush(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "brpoplpush");
        return;
    }
    let Some(block_ms) = parse_timeout(&ctx.args[2]) else {
        append_error(ctx.out, "ERR timeout is not a float or out of range");
        return;
    };
    block_move(
        ctx,
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        false,
        true,
        block_ms,
    )
    .await;
}
