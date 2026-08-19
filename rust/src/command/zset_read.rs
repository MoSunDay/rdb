//! Sorted-set commands, part 2: the read-only family ZCARD/ZSCORE/
//! ZMSCORE/ZCOUNT/ZRANK/ZREVRANK/ZRANDMEMBER. Scores come from the
//! O(1) member records; ZCOUNT windows walk the ordered score index
//! starting at the min bound's sortable prefix; ZRANDMEMBER mirrors
//! `set_scan::srandmember` (negative = with replacement, positive =
//! distinct Fisher-Yates picks). Writes live in `zset_cmd`, pops in
//! `zset_pop`, the ZRANGE family in `zset_range`.

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::zset_util::{
    append_score, collect_scored, eq_ignore_case, parse_score_bound, score_below_min,
    score_past_max, seek_from_sortable, zset_state, ZSetState,
};
use crate::command::Ctx;
use crate::ds::{expire, zset_ds};
use crate::resp::codec::{append_array, append_bulk, append_error, append_int, append_null};
use crate::utils::rand_u64;

/// ZCARD key -> member count (0 when missing).
pub async fn zcard(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "zcard");
        return;
    }
    match zset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        ZSetState::ZSet { count, .. } => append_int(ctx.out, count as i64),
        ZSetState::Missing => append_int(ctx.out, 0),
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
    }
}

/// ZSCORE key member -> the score as a bulk string, or null.
pub async fn zscore(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "zscore");
        return;
    }
    match zset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        ZSetState::Missing => append_null(ctx.out),
        ZSetState::ZSet { .. } => {
            match zset_ds::member_score(
                &ctx.shared.store,
                &ctx.prefix_key,
                &ctx.args[0],
                &ctx.args[1],
            ) {
                Ok(Some(score)) => append_score(ctx.out, score),
                _ => append_null(ctx.out),
            }
        }
    }
}

/// ZMSCORE key member [member ...] -> one bulk score (or null) per
/// member, in argument order.
pub async fn zmscore(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "zmscore");
        return;
    }
    let state = zset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    );
    if let ZSetState::WrongType = state {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    append_array(ctx.out, ctx.args.len() - 1);
    for member in &ctx.args[1..] {
        let score = match state {
            ZSetState::ZSet { .. } => {
                zset_ds::member_score(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], member)
                    .ok()
                    .flatten()
            }
            _ => None,
        };
        match score {
            Some(score) => append_score(ctx.out, score),
            None => append_null(ctx.out),
        }
    }
}

/// ZCOUNT key min max -> members with a score in `[min, max]` (each
/// bound exclusive with a leading `(`). The scan starts at the min
/// bound's sortable prefix and stops as soon as scores pass the max
/// bound, so only the window itself is visited.
pub async fn zcount(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "zcount");
        return;
    }
    let bounds = (
        parse_score_bound(&ctx.args[1]),
        parse_score_bound(&ctx.args[2]),
    );
    let (Some((min, min_incl)), Some((max, max_incl))) = bounds else {
        append_error(ctx.out, "ERR min or max not valid float");
        return;
    };
    match zset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        ZSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        ZSetState::Missing => append_int(ctx.out, 0),
        ZSetState::ZSet { .. } => {
            let key = ctx.args[0].clone();
            let from = seek_from_sortable(min, min_incl).to_be_bytes();
            let mut n = 0u64;
            let _ = zset_ds::for_each_scored(
                &ctx.shared.store,
                &ctx.prefix_key,
                &key,
                &from,
                !min_incl,
                &mut |_, score| {
                    if score_below_min(score, min, min_incl) {
                        return true; // inside the seek, below the window
                    }
                    if score_past_max(score, max, max_incl) {
                        return false; // past the window: stop
                    }
                    n += 1;
                    true
                },
            );
            append_int(ctx.out, n as i64);
        }
    }
}

/// ZRANK/ZREVRANK key member [WITHSCORE] -> 0-based rank from the low
/// (or high, for REV) end; null when the key or member is missing.
async fn rank(ctx: &mut Ctx<'_>, rev: bool, cmd: &str) {
    if ctx.args.len() < 2 || ctx.args.len() > 3 {
        arity(ctx.out, cmd);
        return;
    }
    let withscore = match ctx.args.get(2) {
        None => false,
        Some(opt) if eq_ignore_case(opt, b"WITHSCORE") => true,
        Some(_) => {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    };
    let (key, member) = (ctx.args[0].clone(), ctx.args[1].clone());
    let (count, score) =
        match zset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
            ZSetState::ZSet { count, .. } => (
                count,
                zset_ds::member_score(&ctx.shared.store, &ctx.prefix_key, &key, &member)
                    .ok()
                    .flatten(),
            ),
            ZSetState::Missing => (0, None),
            ZSetState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        };
    let Some(score) = score else {
        if withscore {
            crate::resp::codec::append_raw(ctx.out, b"*-1\r\n");
        } else {
            append_null(ctx.out);
        }
        return;
    };
    let mut suffix = zset_ds::score_sortable(score).to_be_bytes().to_vec();
    suffix.extend_from_slice(&member);
    let rank =
        zset_ds::count_before(&ctx.shared.store, &ctx.prefix_key, &key, &suffix).unwrap_or(0);
    let reply = if rev { count - 1 - rank } else { rank };
    if withscore {
        append_array(ctx.out, 2);
    }
    append_int(ctx.out, reply as i64);
    if withscore {
        append_score(ctx.out, score);
    }
}

/// ZRANK key member [WITHSCORE] -> 0-based ascending rank.
pub async fn zrank(ctx: &mut Ctx<'_>) {
    rank(ctx, false, "zrank").await;
}

/// ZREVRANK key member [WITHSCORE] -> 0-based rank from the high end.
pub async fn zrevrank(ctx: &mut Ctx<'_>) {
    rank(ctx, true, "zrevrank").await;
}

/// ZRANDMEMBER key [count [WITHVALUES]] -> one random member (null when
/// missing), or `count` members: negative counts repeat (with
/// replacement), positive counts give distinct picks; WITHVALUES
/// interleaves each member's score.
pub async fn zrandmember(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 3 {
        arity(ctx.out, "zrandmember");
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
    let withvalues = match ctx.args.get(2) {
        None => false,
        Some(opt) if eq_ignore_case(opt, b"WITHVALUES") => true,
        Some(_) => {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    };
    let key = ctx.args[0].clone();
    let state = zset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms());
    if let ZSetState::WrongType = state {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let items = match state {
        ZSetState::ZSet { .. } => collect_scored(&ctx.shared.store, &ctx.prefix_key, &key),
        _ => Vec::new(),
    };
    let n = items.len();
    let per = 1 + usize::from(withvalues);
    let emit = |out: &mut Vec<u8>, member: &[u8], score: f64| {
        append_bulk(out, member);
        if withvalues {
            append_score(out, score);
        }
    };
    match count {
        None => match n {
            0 => append_null(ctx.out),
            _ => append_bulk(ctx.out, &items[(rand_u64() % n as u64) as usize].0),
        },
        Some(c) => {
            if n == 0 || c == 0 {
                append_array(ctx.out, 0);
                return;
            }
            if c < 0 {
                // Repeating draws: |c| independent picks.
                append_array(ctx.out, c.unsigned_abs() as usize * per);
                for _ in 0..c.unsigned_abs() {
                    let (m, s) = &items[(rand_u64() % n as u64) as usize];
                    emit(ctx.out, m, *s);
                }
            } else {
                // Distinct picks: partial Fisher-Yates over a prefix of
                // length min(c, n) -- the header must match that count.
                let mut idx: Vec<usize> = (0..n).collect();
                let want = (c as usize).min(n);
                for i in 0..want {
                    let j = i + (rand_u64() % (idx.len() - i) as u64) as usize;
                    idx.swap(i, j);
                }
                append_array(ctx.out, want * per);
                for &i in &idx[..want] {
                    emit(ctx.out, &items[i].0, items[i].1);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "zset_read_tests.rs"]
mod zset_read_tests;
