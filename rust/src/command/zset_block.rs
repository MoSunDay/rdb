//! Blocking sorted-set pops BZPOPMIN/BZPOPMAX: the timeout parks on
//! the shared WaitHub via the dedicated park pool (a sync Condvar, run
//! off tokio's shared blocking threads) exactly
//! like `list_block` -- register FIRST, then one latched quick-pop
//! pass over the keys in order, then park -- so a ZADD/STORE notify
//! landing between the read and the park is never lost. Writers notify
//! a zset's meta root after committing (`zset_cmd`/`zsetops_cmd`).
//!
//! The quick-pop removes ONE extreme member (lowest/highest score,
//! member bytes within ties -- the first/last record of the ascending
//! score index) in a single fsync; success replies the flat triple
//! `*3 (key, member, score)` of the popped key.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::list_block::{parse_timeout, MAX_SLICE_MS};
use crate::command::zset_util::{append_score, commit_zset, zset_state, ZSetState};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, setops, wait, zset_ds};
use crate::park;
use crate::resp::codec::{append_bulk, append_error, append_raw};

/// Outcome of a blocking zset pop attempt.
enum ZBlockResult {
    /// One member was popped off `key` (already committed).
    Got {
        key: Vec<u8>,
        member: Vec<u8>,
        score: f64,
    },
    /// The deadline elapsed.
    Timeout,
    /// A key holds a non-zset value; the caller replies WRONGTYPE.
    WrongType,
    /// The commit failed and the error reply is already in `out`.
    Failed,
}

/// Pop ONE extreme member of `key` (first record for MIN, last for
/// MAX) under the caller-held latch; `None` when the zset drained
/// between the state read and the scan (practically unreachable) --
/// the caller then keeps blocking.
async fn quick_pop(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    count: u64,
    max: bool,
    cmd: &str,
) -> Option<ZBlockResult> {
    let mut chosen: Option<(Vec<u8>, f64)> = None;
    let _ = zset_ds::for_each_scored(
        &ctx.shared.store,
        &ctx.prefix_key,
        key,
        b"",
        false,
        &mut |member, score| {
            chosen = Some((member.to_vec(), score));
            max // MIN stops at the first record; MAX scans on, last wins
        },
    );
    let (member, score) = chosen?;
    let mut batch = WriteBatch::default();
    zset_ds::del_member(&mut batch, &ctx.prefix_key, key, &member);
    zset_ds::del_scored(&mut batch, &ctx.prefix_key, key, score, &member);
    if commit_zset(ctx, key, expire_ms, count - 1, batch, cmd).await {
        Some(ZBlockResult::Got {
            key: key.to_vec(),
            member,
            score,
        })
    } else {
        Some(ZBlockResult::Failed)
    }
}

/// Core of BZPOPMIN/BZPOPMAX: cycle register -> latched quick path ->
/// park. Keys are visited in argument order; the first non-empty one
/// yields exactly one member.
async fn block_pop(
    ctx: &mut Ctx<'_>,
    keys: &[Vec<u8>],
    max: bool,
    cmd: &str,
    block_ms: u64,
) -> ZBlockResult {
    // BLOCK 0 means "forever" (`None`): the park runs in MAX_SLICE_MS
    // slices renewed each round. Any other timeout is a hard deadline
    // (checked_add: even a saturated ms count must not panic; an
    // unrepresentable deadline degrades to "block forever").
    let deadline = (block_ms > 0)
        .then(|| Instant::now().checked_add(Duration::from_millis(block_ms)))
        .flatten();
    loop {
        // 1. one shared waiter under every key's root, BEFORE the read.
        let waiter = Arc::new(wait::new_waiter());
        let roots: Vec<Vec<u8>> = keys
            .iter()
            .map(|k| zset_ds::meta_key(&ctx.prefix_key, k))
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
            match zset_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
                ZSetState::WrongType => {
                    for root in &roots {
                        wait::unregister(&ctx.shared.wait_hub, root, &waiter);
                    }
                    return ZBlockResult::WrongType;
                }
                ZSetState::ZSet { expire_ms, count } if count > 0 => {
                    let result = quick_pop(ctx, key, expire_ms, count, max, cmd).await;
                    for root in &roots {
                        wait::unregister(&ctx.shared.wait_hub, root, &waiter);
                    }
                    match result {
                        Some(r) => return r,
                        None => break, // drained mid-check: park and retry
                    }
                }
                ZSetState::ZSet { .. } | ZSetState::Missing => {}
            }
        }
        // 3. deadline check, then a bounded park on the waiter. `None`
        // (BLOCK 0) never elapses: a slice expiry just renews the loop.
        let now = Instant::now();
        if deadline.is_some_and(|d| now >= d) {
            for root in &roots {
                wait::unregister(&ctx.shared.wait_hub, root, &waiter);
            }
            return ZBlockResult::Timeout;
        }
        let left_ms = deadline
            .map_or(MAX_SLICE_MS, |d| {
                d.saturating_duration_since(now).as_millis() as u64
            })
            .min(MAX_SLICE_MS);
        let parked = Arc::clone(&waiter);
        let woke = park::park(move || wait::wait(&parked, Duration::from_millis(left_ms))).await;
        for root in &roots {
            wait::unregister(&ctx.shared.wait_hub, root, &waiter);
        }
        match woke.unwrap_or(wait::WaitOutcome::Timeout) {
            wait::WaitOutcome::Timeout if deadline.is_some_and(|d| Instant::now() >= d) => {
                return ZBlockResult::Timeout
            }
            _ => {} // signaled, spurious, or a renewed forever-slice: loop and re-read
        }
    }
}

/// Shared body: `*3` (key, member, score) on success, a null array on
/// timeout.
async fn block_pop_cmd(ctx: &mut Ctx<'_>, max: bool, cmd: &str) {
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
    match block_pop(ctx, &keys, max, cmd, block_ms).await {
        ZBlockResult::Got { key, member, score } => {
            append_raw(ctx.out, b"*3\r\n");
            append_bulk(ctx.out, &key);
            append_bulk(ctx.out, &member);
            append_score(ctx.out, score);
        }
        ZBlockResult::Timeout => append_raw(ctx.out, b"*-1\r\n"),
        ZBlockResult::WrongType => append_error(ctx.out, WRONGTYPE),
        ZBlockResult::Failed => {}
    }
}

/// BZPOPMIN key [key ...] timeout -> lowest-scored member of the first
/// non-empty key.
pub async fn bzpopmin(ctx: &mut Ctx<'_>) {
    block_pop_cmd(ctx, false, "bzpopmin").await;
}

/// BZPOPMAX key [key ...] timeout -> highest-scored member of the
/// first non-empty key.
pub async fn bzpopmax(ctx: &mut Ctx<'_>) {
    block_pop_cmd(ctx, true, "bzpopmax").await;
}
