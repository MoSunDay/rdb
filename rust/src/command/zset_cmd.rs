//! Sorted-set write commands: ZADD and ZINCRBY. Every member lives as
//! dual records through `ds::zset_ds` (O(1) member lookup plus the
//! ordered score index); mutations land in ONE batched fsync under the
//! per-key latch, and commits that ADDED members wake blocking readers.
//! Shared helpers live in `zset_util`, point reads in `zset_read`,
//! removals/pops in `zset_pop`, the ZRANGE family in `zset_range`.

use std::collections::HashMap;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::arity;
use crate::command::keys_core;
use crate::command::zset_util::{append_score, commit_zset, parse_score, write_meta_of};
use crate::command::Ctx;
use crate::ds::{latch, wait, zset_ds};
use crate::resp::codec::{append_error, append_int, append_null};

/// One member's current score: the in-batch `pending` value when this
/// call already wrote the member (duplicate ZADD members), else the
/// stored record. `Ok(None)` = member absent so far.
fn effective_score(
    ctx: &Ctx<'_>,
    key: &[u8],
    member: &[u8],
    pending: &HashMap<Vec<u8>, f64>,
) -> Result<Option<f64>, String> {
    match pending.get(member) {
        Some(score) => Ok(Some(*score)),
        None => zset_ds::member_score(&ctx.shared.store, &ctx.prefix_key, key, member),
    }
}

/// The six ZADD mode flags, parsed in any order before the pairs.
#[derive(Default)]
struct ZaddFlags {
    nx: bool,
    xx: bool,
    gt: bool,
    lt: bool,
    ch: bool,
    incr: bool,
}

/// Uppercased ZADD option name of `arg`, when it names one.
fn zadd_flag(arg: &[u8]) -> Option<&'static str> {
    let text = std::str::from_utf8(arg).ok()?;
    match text.to_ascii_uppercase().as_str() {
        "NX" => Some("NX"),
        "XX" => Some("XX"),
        "GT" => Some("GT"),
        "LT" => Some("LT"),
        "CH" => Some("CH"),
        "INCR" => Some("INCR"),
        _ => None,
    }
}

/// ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [...] -> count of
/// NEW members (updated ones too with CH -- but a same-score re-add
/// counts as neither new nor changed); with INCR the new score as a
/// bulk string (null when the flags veto the update). All-skipped calls
/// leave a missing key missing (Redis semantics).
pub async fn zadd(ctx: &mut Ctx<'_>) {
    let mut flags = ZaddFlags::default();
    let mut i = 1;
    while i < ctx.args.len() {
        match ctx.args.get(i).and_then(|a| zadd_flag(a)) {
            Some("NX") => flags.nx = true,
            Some("XX") => flags.xx = true,
            Some("GT") => flags.gt = true,
            Some("LT") => flags.lt = true,
            Some("CH") => flags.ch = true,
            Some(_) => flags.incr = true,
            None => break, // first non-option: the score/member pairs start
        }
        i += 1;
    }
    let ZaddFlags {
        nx,
        xx,
        gt,
        lt,
        ch,
        incr,
    } = flags;
    let tail: Vec<Vec<u8>> = ctx.args[i.min(ctx.args.len())..].to_vec();
    if tail.len() < 2 || !tail.len().is_multiple_of(2) {
        arity(ctx.out, "zadd");
        return;
    }
    if nx && xx {
        append_error(
            ctx.out,
            "ERR XX and NX options at the same time are not compatible",
        );
        return;
    }
    if (nx && (gt || lt)) || (gt && lt) {
        append_error(
            ctx.out,
            "ERR GT, LT, and/or NX options at the same time are not compatible",
        );
        return;
    }
    if incr && tail.len() != 2 {
        append_error(
            ctx.out,
            "ERR INCR option supports a single increment-element pair",
        );
        return;
    }
    let mut pairs: Vec<(f64, Vec<u8>)> = Vec::new();
    for pair in tail.chunks(2) {
        let Some(score) = parse_score(&pair[0]) else {
            append_error(ctx.out, "ERR value is not a valid float");
            return;
        };
        pairs.push((score, pair[1].clone()));
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
    if incr {
        zadd_incr(ctx, &key, expire_ms, base, pairs[0].clone(), &flags).await;
        return;
    }
    let mut batch = WriteBatch::default();
    let mut pending: HashMap<Vec<u8>, f64> = HashMap::new();
    let (mut count, mut added, mut changed) = (base, 0u64, 0u64);
    for (score, member) in &pairs {
        let old = match effective_score(ctx, &key, member, &pending) {
            Ok(v) => v,
            Err(_) => {
                append_error(ctx.out, "ERR: zadd failed");
                return;
            }
        };
        if nx && old.is_some() {
            continue; // NX never touches existing members
        }
        if xx && old.is_none() {
            continue; // XX never creates members
        }
        if gt || lt {
            // GT/LT only update existing members, strictly better scores.
            match old {
                None => continue,
                Some(prev) if gt && *score <= prev => continue,
                Some(prev) if lt && *score >= prev => continue,
                _ => {}
            }
        }
        match old {
            Some(prev) if prev == *score => {
                // Unchanged score: counts under neither flag (Redis does
                // not report same-score re-adds as changed) and there is
                // nothing to rewrite -- the stored records already say
                // exactly this (member, score) pair.
                pending.insert(member.clone(), *score);
                continue;
            }
            Some(prev) => {
                zset_ds::del_scored(&mut batch, &ctx.prefix_key, &key, prev, member);
                changed += 1;
            }
            None => {
                // New members count as added AND changed: `changed`
                // subsumes `added`, so it alone is the CH reply.
                added += 1;
                changed += 1;
                count += 1;
            }
        }
        zset_ds::put_member(&mut batch, &ctx.prefix_key, &key, member, *score);
        zset_ds::put_scored(&mut batch, &ctx.prefix_key, &key, *score, member);
        pending.insert(member.clone(), *score);
    }
    if added + changed == 0 {
        append_int(ctx.out, 0); // nothing applied: a missing key stays missing
        return;
    }
    if commit_zset(ctx, &key, expire_ms, count, batch, "zadd").await {
        if added > 0 {
            wait::notify_n(
                &ctx.shared.wait_hub,
                &zset_ds::meta_key(&ctx.prefix_key, &key),
                added as usize,
            );
        }
        append_int(ctx.out, if ch { changed as i64 } else { added as i64 });
    }
}

/// ZADD ... INCR score member: at most one update; flags vetoing the
/// change reply null instead of a score.
async fn zadd_incr(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    base: u64,
    pair: (f64, Vec<u8>),
    flags: &ZaddFlags,
) {
    let (delta, member) = pair;
    let (nx, xx, gt, lt) = (flags.nx, flags.xx, flags.gt, flags.lt);
    let old = match zset_ds::member_score(&ctx.shared.store, &ctx.prefix_key, key, &member) {
        Ok(v) => v,
        Err(_) => {
            append_error(ctx.out, "ERR: zadd failed");
            return;
        }
    };
    if (nx && old.is_some()) || (xx && old.is_none()) || ((gt || lt) && old.is_none()) {
        append_null(ctx.out); // GT/LT never create members, INCR honours that
        return;
    }
    // A MISSING member takes delta verbatim: `0.0 + (-0.0)` collapses
    // to +0.0 and would lose the sign INCR must preserve ("-0").
    let result = match old {
        None => delta,
        Some(prev) => prev + delta,
    };
    if result.is_nan() {
        append_error(ctx.out, "ERR resulting score is not a number");
        return;
    }
    if let Some(prev) = old {
        if (gt && result <= prev) || (lt && result >= prev) {
            append_null(ctx.out);
            return;
        }
    }
    let mut batch = WriteBatch::default();
    if let Some(prev) = old {
        zset_ds::del_scored(&mut batch, &ctx.prefix_key, key, prev, &member);
    }
    zset_ds::put_member(&mut batch, &ctx.prefix_key, key, &member, result);
    zset_ds::put_scored(&mut batch, &ctx.prefix_key, key, result, &member);
    if commit_zset(
        ctx,
        key,
        expire_ms,
        base + u64::from(old.is_none()),
        batch,
        "zadd",
    )
    .await
    {
        if old.is_none() {
            wait::notify(
                &ctx.shared.wait_hub,
                &zset_ds::meta_key(&ctx.prefix_key, key),
            );
        }
        append_score(ctx.out, result);
    }
}

/// ZINCRBY key increment member -> the new score as a bulk string.
pub async fn zincrby(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "zincrby");
        return;
    }
    let Some(delta) = parse_score(&ctx.args[1]) else {
        append_error(ctx.out, "ERR value is not a valid float");
        return;
    };
    let (key, member) = (ctx.args[0].clone(), ctx.args[2].clone());
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    let old = match zset_ds::member_score(&ctx.shared.store, &ctx.prefix_key, &key, &member) {
        Ok(v) => v,
        Err(_) => {
            append_error(ctx.out, "ERR: zincrby failed");
            return;
        }
    };
    // Missing member -> delta itself: `0.0 + (-0.0)` is +0.0 and would
    // drop the "-0" sign ZSCORE must echo back.
    let result = match old {
        None => delta,
        Some(prev) => prev + delta,
    };
    if result.is_nan() {
        append_error(ctx.out, "ERR resulting score is not a number");
        return;
    }
    let mut batch = WriteBatch::default();
    if let Some(prev) = old {
        zset_ds::del_scored(&mut batch, &ctx.prefix_key, &key, prev, &member);
    }
    zset_ds::put_member(&mut batch, &ctx.prefix_key, &key, &member, result);
    zset_ds::put_scored(&mut batch, &ctx.prefix_key, &key, result, &member);
    if commit_zset(
        ctx,
        &key,
        expire_ms,
        base + u64::from(old.is_none()),
        batch,
        "zincrby",
    )
    .await
    {
        if old.is_none() {
            wait::notify(
                &ctx.shared.wait_hub,
                &zset_ds::meta_key(&ctx.prefix_key, &key),
            );
        }
        append_score(ctx.out, result);
    }
}

#[cfg(test)]
#[path = "zset_tests.rs"]
mod zset_tests;
