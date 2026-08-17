//! Sorted-set range commands: ZRANGE (rank/BYSCORE/BYLEX modes with
//! REV/LIMIT/WITHSCORES) plus the classic twins ZRANGEBYSCORE/
//! ZREVRANGEBYSCORE, ZRANGEBYLEX/ZREVRANGEBYLEX and ZLEXCOUNT. Score
//! windows walk `zset_ds::for_each_scored` from the min bound's
//! sortable prefix (stopping past the max bound); lex windows scan the
//! whole index and filter member bytes; REV variants collect forward,
//! reverse, THEN apply LIMIT -- the offset counts in reply order.

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::list_cmd::clamp_range;
use crate::command::zset_util::{
    append_score, collect_scored, lex_within, parse_lex_bound, parse_score_bound, zset_state,
    LexBound, ZSetState,
};
use crate::command::Ctx;
use crate::ds::{expire, zset_ds};
use crate::resp::codec::{append_array, append_bulk, append_error, append_int};

/// Parsed ZRANGE-family trailing options (any order, case-insensitive).
#[derive(Default)]
struct RangeOpts {
    by_score: bool,
    by_lex: bool,
    rev: bool,
    withscores: bool,
    limit: Option<(i64, i64)>,
}

/// Parse the trailing options. `modes` allows the BYSCORE/BYLEX/REV
/// switches (ZRANGE only); anything unknown, or a broken LIMIT, replies
/// `-ERR` and answers `None`.
fn parse_range_opts(out: &mut Vec<u8>, args: &[Vec<u8>], modes: bool) -> Option<RangeOpts> {
    let mut opts = RangeOpts::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_slice();
        let upper = |kw: &[u8]| crate::command::zset_util::eq_ignore_case(arg, kw);
        if modes && upper(b"BYSCORE") {
            opts.by_score = true;
        } else if modes && upper(b"BYLEX") {
            opts.by_lex = true;
        } else if modes && upper(b"REV") {
            opts.rev = true;
        } else if upper(b"WITHSCORES") {
            opts.withscores = true;
        } else if upper(b"LIMIT") {
            if i + 2 >= args.len() {
                append_error(out, "ERR syntax error");
                return None;
            }
            let parsed = (parse_i64(&args[i + 1]), parse_i64(&args[i + 2]));
            let (Some(offset), Some(count)) = parsed else {
                append_error(out, "ERR value is not an integer or out of range");
                return None;
            };
            opts.limit = Some((offset, count));
            i += 2;
        } else {
            append_error(out, "ERR syntax error");
            return None;
        }
        i += 1;
    }
    Some(opts)
}

/// Emit `items` (already in reply order): LIMIT skips `offset` entries
/// then takes `count` (negative = all the rest), WITHSCORES interleaves
/// each member's score.
fn emit(out: &mut Vec<u8>, items: &[(Vec<u8>, f64)], withscores: bool, limit: Option<(i64, i64)>) {
    let (offset, count) = limit.unwrap_or((0, -1));
    let offset = offset.max(0) as usize; // Redis clamps negative offsets
    let end = match count {
        n if n < 0 => items.len(),
        n => offset.saturating_add(n as usize).min(items.len()),
    };
    let slice = items.get(offset..end).unwrap_or(&[]);
    let per = 1 + usize::from(withscores);
    append_array(out, slice.len() * per);
    for (member, score) in slice {
        append_bulk(out, member);
        if withscores {
            append_score(out, *score);
        }
    }
}

/// All members with scores in `[min, max]` (each bound honoring its
/// inclusivity), ascending: the scan seeks the min bound's sortable
/// prefix and stops as soon as scores pass the max bound.
fn collect_score_window(
    ctx: &Ctx<'_>,
    key: &[u8],
    (min, min_incl): (f64, bool),
    (max, max_incl): (f64, bool),
) -> Vec<(Vec<u8>, f64)> {
    let mut items = Vec::new();
    let from = zset_ds::score_sortable(min).to_be_bytes();
    let _ = zset_ds::for_each_scored(
        &ctx.shared.store,
        &ctx.prefix_key,
        key,
        &from,
        !min_incl,
        &mut |member, score| {
            if score == min && !min_incl {
                return true; // inside the seek, below the window
            }
            if score > max || (score == max && !max_incl) {
                return false; // past the window: stop
            }
            items.push((member.to_vec(), score));
            true
        },
    );
    items
}

/// Shared BYSCORE core: collect the window, reverse when `rev`, emit.
fn score_window_reply(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    min: (f64, bool),
    max: (f64, bool),
    rev: bool,
    withscores: bool,
    limit: Option<(i64, i64)>,
) {
    match zset_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        ZSetState::Missing => append_array(ctx.out, 0),
        ZSetState::ZSet { .. } => {
            let mut items = collect_score_window(ctx, key, min, max);
            if rev {
                items.reverse();
            }
            emit(ctx.out, &items, withscores, limit);
        }
    }
}

/// Shared BYLEX core: filter the whole index by member bytes, reverse
/// when `rev`, emit (WITHSCORES never legal here, callers enforce).
fn lex_window_reply(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    min: &LexBound,
    max: &LexBound,
    rev: bool,
    limit: Option<(i64, i64)>,
) {
    match zset_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        ZSetState::Missing => append_array(ctx.out, 0),
        ZSetState::ZSet { .. } => {
            let mut items: Vec<(Vec<u8>, f64)> =
                collect_scored(&ctx.shared.store, &ctx.prefix_key, key)
                    .into_iter()
                    .filter(|(member, _)| lex_within(member, min, max))
                    .collect();
            if rev {
                items.reverse();
            }
            emit(ctx.out, &items, false, limit);
        }
    }
}

/// ZRANGE key start stop [BYSCORE|BYLEX] [REV] [LIMIT o c] [WITHSCORES]:
/// rank positions (negatives from the back) by default; BYSCORE takes
/// score bounds (max then min under REV, Redis 6.2 semantics); BYLEX
/// takes lex bounds over one-score zsets.
pub async fn zrange(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "zrange");
        return;
    }
    let Some(opts) = parse_range_opts(ctx.out, &ctx.args[3..], true) else {
        return;
    };
    if opts.by_score && opts.by_lex {
        append_error(ctx.out, "ERR syntax error");
        return;
    }
    if opts.limit.is_some() && !opts.by_score && !opts.by_lex {
        append_error(
            ctx.out,
            "ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX",
        );
        return;
    }
    if opts.by_lex && opts.withscores {
        append_error(
            ctx.out,
            "ERR syntax error, WITHSCORES not supported in combination with BYLEX",
        );
        return;
    }
    let key = ctx.args[0].clone();
    if opts.by_lex {
        let parsed = (parse_lex_bound(&ctx.args[1]), parse_lex_bound(&ctx.args[2]));
        let (Some(min), Some(max)) = parsed else {
            append_error(ctx.out, "ERR min or max not valid string range item");
            return;
        };
        lex_window_reply(ctx, &key, &min, &max, opts.rev, opts.limit);
        return;
    }
    if opts.by_score {
        // Under REV the arguments arrive as (max, min).
        let (min_arg, max_arg) = if opts.rev {
            (&ctx.args[2], &ctx.args[1])
        } else {
            (&ctx.args[1], &ctx.args[2])
        };
        let parsed = (parse_score_bound(min_arg), parse_score_bound(max_arg));
        let (Some(min), Some(max)) = parsed else {
            append_error(ctx.out, "ERR min or max not valid float");
            return;
        };
        score_window_reply(ctx, &key, min, max, opts.rev, opts.withscores, opts.limit);
        return;
    }
    let (Some(start), Some(stop)) = (parse_i64(&ctx.args[1]), parse_i64(&ctx.args[2])) else {
        append_error(ctx.out, "ERR value is not an integer or out of range");
        return;
    };
    rank_window_reply(ctx, &key, start, stop, opts.rev, opts.withscores);
}

/// Rank mode: `[start..=stop]` with Redis clamping (negatives from the
/// back, empty selection -> `*0`); REV walks the same window but emits
/// descending.
fn rank_window_reply(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    start: i64,
    stop: i64,
    rev: bool,
    withscores: bool,
) {
    match zset_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        ZSetState::Missing => append_array(ctx.out, 0),
        ZSetState::ZSet { count, .. } => {
            let Some((from, to)) = clamp_range(start, stop, count) else {
                append_array(ctx.out, 0);
                return;
            };
            let mut items = collect_scored(&ctx.shared.store, &ctx.prefix_key, key);
            if rev {
                items.reverse();
            }
            match items.get(from as usize..=to as usize) {
                Some(slice) => emit(ctx.out, slice, withscores, None),
                None => append_array(ctx.out, 0),
            }
        }
    }
}

/// ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT o c] -> ascending.
pub async fn zrangebyscore(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "zrangebyscore");
        return;
    }
    let Some(opts) = parse_range_opts(ctx.out, &ctx.args[3..], false) else {
        return;
    };
    let parsed = (
        parse_score_bound(&ctx.args[1]),
        parse_score_bound(&ctx.args[2]),
    );
    let (Some(min), Some(max)) = parsed else {
        append_error(ctx.out, "ERR min or max not valid float");
        return;
    };
    let key = ctx.args[0].clone();
    score_window_reply(ctx, &key, min, max, false, opts.withscores, opts.limit);
}

/// ZREVRANGEBYSCORE key max min [WITHSCORES] [LIMIT o c] -> descending;
/// the bounds arrive highest first.
pub async fn zrevrangebyscore(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "zrevrangebyscore");
        return;
    }
    let Some(opts) = parse_range_opts(ctx.out, &ctx.args[3..], false) else {
        return;
    };
    let parsed = (
        parse_score_bound(&ctx.args[2]),
        parse_score_bound(&ctx.args[1]),
    );
    let (Some(min), Some(max)) = parsed else {
        append_error(ctx.out, "ERR min or max not valid float");
        return;
    };
    let key = ctx.args[0].clone();
    score_window_reply(ctx, &key, min, max, true, opts.withscores, opts.limit);
}

/// ZRANGEBYLEX key min max [LIMIT o c] -> members in byte order.
pub async fn zrangebylex(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "zrangebylex");
        return;
    }
    let Some(opts) = parse_range_opts(ctx.out, &ctx.args[3..], false) else {
        return;
    };
    let parsed = (parse_lex_bound(&ctx.args[1]), parse_lex_bound(&ctx.args[2]));
    let (Some(min), Some(max)) = parsed else {
        append_error(ctx.out, "ERR min or max not valid string range item");
        return;
    };
    let key = ctx.args[0].clone();
    lex_window_reply(ctx, &key, &min, &max, false, opts.limit);
}

/// ZREVRANGEBYLEX key max min [LIMIT o c] -> members in reverse byte
/// order; the bounds arrive highest first.
pub async fn zrevrangebylex(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "zrevrangebylex");
        return;
    }
    let Some(opts) = parse_range_opts(ctx.out, &ctx.args[3..], false) else {
        return;
    };
    let parsed = (parse_lex_bound(&ctx.args[2]), parse_lex_bound(&ctx.args[1]));
    let (Some(min), Some(max)) = parsed else {
        append_error(ctx.out, "ERR min or max not valid string range item");
        return;
    };
    let key = ctx.args[0].clone();
    lex_window_reply(ctx, &key, &min, &max, true, opts.limit);
}

/// ZLEXCOUNT key min max -> members within the lex bounds.
pub async fn zlexcount(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "zlexcount");
        return;
    }
    let parsed = (parse_lex_bound(&ctx.args[1]), parse_lex_bound(&ctx.args[2]));
    let (Some(min), Some(max)) = parsed else {
        append_error(ctx.out, "ERR min or max not valid string range item");
        return;
    };
    match zset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        _ => {
            let n = collect_scored(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0])
                .iter()
                .filter(|(member, _)| lex_within(member, &min, &max))
                .count();
            append_int(ctx.out, n as i64);
        }
    }
}

#[cfg(test)]
#[path = "zset_range_tests.rs"]
mod zset_range_tests;
