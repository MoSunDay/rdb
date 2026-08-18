//! Whole-list rewrites: LREM (drop matching values) and LTRIM (keep one
//! window), both rebuilding sides densely via `compact_side` in ONE
//! fsync per command (`list_cmd::commit_list` shape); a list left empty
//! is deleted.

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::list_cmd::{clamp_range, commit_list, list_state, ListState};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, list_ds};
use crate::resp::codec::{append_error, append_int, append_string};

/// One side's live entries with their physical index.
type SideEntries = Vec<(u64, Vec<u8>)>;

/// Both sides in logical order: L entries with index (descending `l` =
/// head to tail of the left part), then R entries (ascending `r`).
fn sides(
    ctx: &Ctx<'_>,
    key: &[u8],
    meta: &list_ds::ListMeta,
) -> Result<(SideEntries, SideEntries), String> {
    let l = list_ds::collect_side(&ctx.shared.store, &ctx.prefix_key, key, meta, true)?;
    let r = list_ds::collect_side(&ctx.shared.store, &ctx.prefix_key, key, meta, false)?;
    Ok((l, r))
}

/// Rewrite one side densely after dropping `kill` indices: every delete
/// (dropped slots plus survivors' OLD slots) enters the batch BEFORE the
/// survivor re-puts, so the batch never observes a half-moved window.
/// Survivors keep their logical order; the L side renumbers from
/// `l_next - 1` downward, the R side from the new base upward. Returns
/// the side's new count.
fn compact_side(
    batch: &mut WriteBatch,
    prefix: &[u8],
    key: &[u8],
    live: &[(u64, Vec<u8>)],
    kill: &[u64],
    left: bool,
) -> u64 {
    // `next`: highest live index + 1 (the L side is collected
    // descending, so FIRST = newest).
    let next = match live.first() {
        Some((i, _)) if left => i + 1,
        Some(_) => live.last().expect("non-empty side").0 + 1,
        None => return 0,
    };
    let kept: Vec<(u64, Vec<u8>)> = live
        .iter()
        .filter(|(i, _)| !kill.contains(i))
        .cloned()
        .collect();
    let count = kept.len() as u64;
    let target = |i: usize| {
        if left {
            next - 1 - i as u64
        } else {
            next - count + i as u64
        }
    };
    for i in kill {
        if left {
            list_ds::del_l(batch, prefix, key, *i);
        } else {
            list_ds::del_r(batch, prefix, key, *i);
        }
    }
    for (i, (old, _)) in kept.iter().enumerate() {
        if *old != target(i) {
            // moved survivor: its old slot must go too
            if left {
                list_ds::del_l(batch, prefix, key, *old);
            } else {
                list_ds::del_r(batch, prefix, key, *old);
            }
        }
    }
    for (i, (_, elem)) in kept.iter().enumerate() {
        if left {
            list_ds::put_l(batch, prefix, key, target(i), elem);
        } else {
            list_ds::put_r(batch, prefix, key, target(i), elem);
        }
    }
    count
}

/// LREM key count element -> count of removed copies: from the head
/// when count > 0, from the tail when count < 0, all when count == 0.
pub async fn lrem(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "lrem");
        return;
    }
    let (key, cnt, elem) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    let Some(cnt) = parse_i64(&cnt) else {
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
                append_int(ctx.out, 0);
                return;
            }
            ListState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        };
    let (lside, rside) = match sides(ctx, &key, &meta) {
        Ok(v) => v,
        Err(_) => {
            append_error(ctx.out, "ERR: lrem failed");
            return;
        }
    };
    // Visit head-to-tail (count >= 0) or tail-to-head (count < 0) and
    // mark up to `budget` matching entries for deletion.
    let mut budget = if cnt == 0 {
        u64::MAX
    } else {
        cnt.unsigned_abs()
    };
    let mut visit: Vec<(bool, u64, &Vec<u8>)> = lside
        .iter()
        .map(|(i, e)| (true, *i, e))
        .chain(rside.iter().map(|(i, e)| (false, *i, e)))
        .collect();
    if cnt < 0 {
        visit.reverse();
    }
    let mut kill_l: Vec<u64> = Vec::new();
    let mut kill_r: Vec<u64> = Vec::new();
    for (is_left, idx, e) in &visit {
        if budget == 0 {
            break;
        }
        if **e == elem {
            if *is_left {
                kill_l.push(*idx);
            } else {
                kill_r.push(*idx);
            }
            budget -= 1;
        }
    }
    let removed = kill_l.len() + kill_r.len();
    let mut after = meta;
    let mut batch = WriteBatch::default();
    if !kill_l.is_empty() {
        after.l_count = compact_side(&mut batch, &ctx.prefix_key, &key, &lside, &kill_l, true);
    }
    if !kill_r.is_empty() {
        after.r_count = compact_side(&mut batch, &ctx.prefix_key, &key, &rside, &kill_r, false);
    }
    let emptied = after.is_empty();
    if commit_list(
        ctx,
        &key,
        expire_ms,
        if emptied { None } else { Some(&after) },
        batch,
        "lrem",
    )
    .await
    {
        append_int(ctx.out, removed as i64);
    }
}

/// LTRIM key start stop: keep only `[start..=stop]` (negatives resolve
/// like LRANGE); trimming to nothing deletes the key.
pub async fn ltrim(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "ltrim");
        return;
    }
    let (key, start, stop) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    let (Some(start), Some(stop)) = (parse_i64(&start), parse_i64(&stop)) else {
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
                append_string(ctx.out, "OK");
                return;
            }
            ListState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        };
    let Some((from, to)) = clamp_range(start, stop, meta.len()) else {
        // empty selection: the whole list goes
        if commit_list(ctx, &key, expire_ms, None, WriteBatch::default(), "ltrim").await {
            append_string(ctx.out, "OK");
        }
        return;
    };
    let (lside, rside) = match sides(ctx, &key, &meta) {
        Ok(v) => v,
        Err(_) => {
            append_error(ctx.out, "ERR: ltrim failed");
            return;
        }
    };
    // Kill every entry whose logical position falls outside [from, to].
    let mut kill_l: Vec<u64> = Vec::new();
    for (i, _) in &lside {
        let p = meta.l_base() + meta.l_count - 1 - *i;
        if p < from || p > to {
            kill_l.push(*i);
        }
    }
    let mut kill_r: Vec<u64> = Vec::new();
    for (i, _) in &rside {
        let p = meta.l_count + (*i - meta.r_base());
        if p < from || p > to {
            kill_r.push(*i);
        }
    }
    let mut after = meta;
    let mut batch = WriteBatch::default();
    if !kill_l.is_empty() {
        after.l_count = compact_side(&mut batch, &ctx.prefix_key, &key, &lside, &kill_l, true);
    }
    if !kill_r.is_empty() {
        after.r_count = compact_side(&mut batch, &ctx.prefix_key, &key, &rside, &kill_r, false);
    }
    let emptied = after.is_empty();
    if commit_list(
        ctx,
        &key,
        expire_ms,
        if emptied { None } else { Some(&after) },
        batch,
        "ltrim",
    )
    .await
    {
        append_string(ctx.out, "OK");
    }
}
