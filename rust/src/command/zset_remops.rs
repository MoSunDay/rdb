//! Sorted-set range removals: ZREMRANGEBYRANK/ZREMRANGEBYSCORE/
//! ZREMRANGEBYLEX. Each collects the doomed window under the per-key
//! latch, then deletes BOTH records of every member plus the meta
//! update in ONE fsync (`commit_zset` wipes the family once the count
//! hits zero -- an empty zset does not exist). Removals never add
//! members, so no blocking reader is woken.

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64};
use crate::command::list_cmd::clamp_range;
use crate::command::zset_util::{
    collect_scored, commit_zset, lex_within, parse_lex_bound, parse_score_bound, score_below_min,
    score_past_max, seek_from_sortable, write_meta_of,
};
use crate::command::{keys_core, Ctx};
use crate::ds::{latch, zset_ds};
use crate::resp::codec::{append_error, append_int};

/// Shared removal core: delete every `(member, score)` pair of
/// `window` in one batch, drop the count accordingly and reply the
/// number removed. An empty window touches nothing and replies `:0`.
async fn remove_window(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    base: u64,
    cmd: &str,
    window: Vec<(Vec<u8>, f64)>,
) {
    if window.is_empty() {
        append_int(ctx.out, 0);
        return;
    }
    let mut batch = WriteBatch::default();
    for (member, score) in &window {
        zset_ds::del_member(&mut batch, &ctx.prefix_key, key, member);
        zset_ds::del_scored(&mut batch, &ctx.prefix_key, key, *score, member);
    }
    if commit_zset(ctx, key, expire_ms, base - window.len() as u64, batch, cmd).await {
        append_int(ctx.out, window.len() as i64);
    }
}

/// ZREMRANGEBYRANK key start stop -> members removed by 0-based rank
/// (negatives count from the back, out-of-range clamps; an empty
/// window removes nothing).
pub async fn zremrangebyrank(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "zremrangebyrank");
        return;
    }
    let (Some(start), Some(stop)) = (parse_i64(&ctx.args[1]), parse_i64(&ctx.args[2])) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
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
        append_int(ctx.out, 0);
        return;
    }
    let Some((lo, hi)) = clamp_range(start, stop, base) else {
        append_int(ctx.out, 0); // clamped to an empty window
        return;
    };
    // Ascending scan with a rank counter; stops once past `hi`.
    let mut window: Vec<(Vec<u8>, f64)> = Vec::new();
    let mut rank = 0u64;
    let _ = zset_ds::for_each_scored(
        &ctx.shared.store,
        &ctx.prefix_key,
        &key,
        b"",
        false,
        &mut |member, score| {
            if rank > hi {
                return false; // past the window: stop
            }
            if rank >= lo {
                window.push((member.to_vec(), score));
            }
            rank += 1;
            true
        },
    );
    remove_window(ctx, &key, expire_ms, base, "zremrangebyrank", window).await;
}

/// ZREMRANGEBYSCORE key min max -> members with a score in the
/// (possibly exclusive, `(`-prefixed) window removed.
pub async fn zremrangebyscore(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "zremrangebyscore");
        return;
    }
    let parsed = (
        parse_score_bound(&ctx.args[1]),
        parse_score_bound(&ctx.args[2]),
    );
    let (Some((min, min_incl)), Some((max, max_incl))) = parsed else {
        append_error(ctx.out, "ERR min or max not valid float");
        return;
    };
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
        append_int(ctx.out, 0);
        return;
    }
    // Seek to the min bound's sortable prefix, filter to the window.
    // An INCLUSIVE zero min must seek at sortable(-0.0): -0.0 members
    // sort below sortable(+0.0) and would silently dodge the removal.
    let from = seek_from_sortable(min, min_incl).to_be_bytes();
    let mut window: Vec<(Vec<u8>, f64)> = Vec::new();
    let _ = zset_ds::for_each_scored(
        &ctx.shared.store,
        &ctx.prefix_key,
        &key,
        &from,
        !min_incl,
        &mut |member, score| {
            if score_below_min(score, min, min_incl) {
                return true; // inside the seek, below the window
            }
            if score_past_max(score, max, max_incl) {
                return false; // past the window: stop
            }
            window.push((member.to_vec(), score));
            true
        },
    );
    remove_window(ctx, &key, expire_ms, base, "zremrangebyscore", window).await;
}

/// ZREMRANGEBYLEX key min max -> members whose bytes fall between the
/// lex bounds (`-`/`+` infinite ends, `[x`/`(x` include/exclude)
/// removed; scores are irrelevant to the window.
pub async fn zremrangebylex(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "zremrangebylex");
        return;
    }
    let parsed = (parse_lex_bound(&ctx.args[1]), parse_lex_bound(&ctx.args[2]));
    let (Some(min), Some(max)) = parsed else {
        append_error(ctx.out, "ERR min or max not valid string range item");
        return;
    };
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
        append_int(ctx.out, 0);
        return;
    }
    let window: Vec<(Vec<u8>, f64)> = collect_scored(&ctx.shared.store, &ctx.prefix_key, &key)
        .into_iter()
        .filter(|(member, _)| lex_within(member, &min, &max))
        .collect();
    remove_window(ctx, &key, expire_ms, base, "zremrangebylex", window).await;
}

#[cfg(test)]
#[path = "zset_ops_tests.rs"]
mod zset_ops_tests;
