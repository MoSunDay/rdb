//! LINSERT (insert beside a pivot, shifting one side's index window),
//! LPOS (match scan with RANK/COUNT/MAXLEN) and LMOVE/RPOPLPUSH (pop
//! off one key, push onto another, ONE fsync for both; same-slot only).

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::list_cmd::{
    blank_meta, commit_list, list_state, lock_sorted, pop_one, ListState,
};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, list_ds, setops, wait};
use crate::resp::codec::{append_bulk, append_error, append_int, append_null};
use crate::store::ops;

/// LEFT/RIGHT tokens (case-insensitive) -> `(pop_left, push_left)`.
pub(crate) fn parse_dirs(pop: &[u8], push: &[u8]) -> Option<(bool, bool)> {
    let pop = match pop.to_ascii_uppercase().as_slice() {
        b"LEFT" => true,
        b"RIGHT" => false,
        _ => return None,
    };
    let push = match push.to_ascii_uppercase().as_slice() {
        b"LEFT" => true,
        b"RIGHT" => false,
        _ => return None,
    };
    Some((pop, push))
}

/// LINSERT key BEFORE|AFTER pivot element -> new length; 0 when the key
/// is missing, -1 when the pivot is absent. Entries between head and
/// insert point shift one slot away; the new one takes the vacated slot.
pub async fn linsert(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        arity(ctx.out, "linsert");
        return;
    }
    let (key, dir, pivot, elem) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
        ctx.args[3].clone(),
    );
    let before = match dir.to_ascii_uppercase().as_slice() {
        b"BEFORE" => true,
        b"AFTER" => false,
        _ => {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
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
    let (Ok(lside), Ok(rside)) = (
        list_ds::collect_side(&ctx.shared.store, &ctx.prefix_key, &key, &meta, true),
        list_ds::collect_side(&ctx.shared.store, &ctx.prefix_key, &key, &meta, false),
    ) else {
        append_error(ctx.out, "ERR: linsert failed");
        return;
    };
    let seq: Vec<&Vec<u8>> = lside
        .iter()
        .map(|(_, e)| e)
        .chain(rside.iter().map(|(_, e)| e))
        .collect();
    let Some(p) = seq.iter().position(|e| **e == pivot) else {
        append_int(ctx.out, -1);
        return;
    };
    let at = p + usize::from(!before);
    let mut after = meta;
    let mut batch = WriteBatch::default();
    if at < lside.len() {
        // Into the L window: entries BEFORE the insert point shift one
        // slot toward the head (l+1); the new one takes `l_target`.
        for (idx, _) in &lside[..at] {
            list_ds::del_l(&mut batch, &ctx.prefix_key, &key, *idx);
        }
        for (idx, e) in lside[..at].iter().rev() {
            list_ds::put_l(&mut batch, &ctx.prefix_key, &key, idx + 1, e);
        }
        let l_target = meta.l_next - at as u64;
        list_ds::put_l(&mut batch, &ctx.prefix_key, &key, l_target, &elem);
        after.l_next += 1;
        after.l_count += 1;
    } else {
        // Into the R window: entries logically AT/AFTER the insert point
        // shift toward the tail (r+1); the new element takes r_target.
        let take = at - lside.len();
        for (idx, _) in &rside[take..] {
            list_ds::del_r(&mut batch, &ctx.prefix_key, &key, *idx);
        }
        for (idx, e) in rside[take..].iter().rev() {
            list_ds::put_r(&mut batch, &ctx.prefix_key, &key, idx + 1, e);
        }
        let r_target = meta.r_base() + take as u64;
        list_ds::put_r(&mut batch, &ctx.prefix_key, &key, r_target, &elem);
        after.r_next += 1;
        after.r_count += 1;
    }
    let len = after.len();
    if commit_list(ctx, &key, expire_ms, Some(&after), batch, "linsert").await {
        wait::notify(
            &ctx.shared.wait_hub,
            &list_ds::meta_key(&ctx.prefix_key, &key),
        );
        append_int(ctx.out, len as i64);
    }
}

/// LPOS key element [RANK r] [COUNT c] [MAXLEN m]: logical positions of
/// matches. RANK skips earlier matches (negative = scan from the tail),
/// COUNT caps the reply as an array; MAXLEN bounds the scan (0 = all).
pub async fn lpos(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "lpos");
        return;
    }
    let (key, elem) = (ctx.args[0].clone(), ctx.args[1].clone());
    let mut rank: i64 = 1;
    let mut count_opt: Option<i64> = None;
    let mut maxlen: i64 = 0;
    let mut i = 2;
    while i < ctx.args.len() {
        let value = match ctx.args.get(i + 1) {
            Some(v) => v,
            None => {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        };
        match ctx.args[i].to_ascii_uppercase().as_slice() {
            b"RANK" => rank = parse_i64(value).unwrap_or(i64::MIN),
            b"COUNT" => count_opt = Some(parse_i64(value).unwrap_or(i64::MIN)),
            b"MAXLEN" => maxlen = parse_i64(value).unwrap_or(i64::MIN),
            _ => {
                append_error(ctx.out, "ERR syntax error");
                return;
            }
        }
        i += 2;
    }
    if rank == 0 {
        append_error(ctx.out, "ERR RANK can't be zero");
        return;
    }
    if count_opt.is_some_and(|c| c < 0) {
        append_error(ctx.out, "ERR COUNT can't be negative");
        return;
    }
    if maxlen < 0 {
        append_error(ctx.out, "ERR MAXLEN can't be negative");
        return;
    }
    let no_match = |ctx: &mut Ctx<'_>| match count_opt {
        None => append_null(ctx.out),
        Some(_) => crate::resp::codec::append_array(ctx.out, 0),
    };
    let meta = match list_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        ListState::List { meta, .. } => meta,
        ListState::Missing => {
            no_match(ctx);
            return;
        }
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let (Ok(lside), Ok(rside)) = (
        list_ds::collect_side(&ctx.shared.store, &ctx.prefix_key, &key, &meta, true),
        list_ds::collect_side(&ctx.shared.store, &ctx.prefix_key, &key, &meta, false),
    ) else {
        append_error(ctx.out, "ERR: lpos failed");
        return;
    };
    // (logical position, element) head to tail, bounded by MAXLEN.
    let l_len = lside.len();
    let mut entries: Vec<(usize, &Vec<u8>)> = lside
        .iter()
        .enumerate()
        .map(|(p, (_, e))| (p, e))
        .chain(rside.iter().enumerate().map(|(p, (_, e))| (l_len + p, e)))
        .collect();
    if maxlen > 0 {
        let m = maxlen as usize;
        if rank < 0 {
            let skip = entries.len().saturating_sub(m);
            entries.drain(..skip);
        } else {
            entries.truncate(m);
        }
    }
    if rank < 0 {
        entries.reverse();
    }
    let skip = (rank.unsigned_abs() - 1) as usize;
    // COUNT 0 asks for every match; absent COUNT wants exactly one.
    let take = match count_opt {
        None => 1,
        Some(0) => usize::MAX,
        Some(c) => c as usize,
    };
    let mut picked: Vec<i64> = Vec::new();
    let mut seen = 0usize;
    for (p, e) in &entries {
        if **e == elem {
            seen += 1;
            if seen > skip {
                picked.push(*p as i64);
                if picked.len() == take {
                    break;
                }
            }
        }
    }
    match count_opt {
        None => match picked.first() {
            Some(p) => append_int(ctx.out, *p),
            None => append_null(ctx.out),
        },
        Some(_) => {
            crate::resp::codec::append_array(ctx.out, picked.len());
            for p in &picked {
                append_int(ctx.out, *p);
            }
        }
    }
}

/// Shared body of LMOVE/RPOPLPUSH: pop one element off `src`, push onto
/// `dst`, ONE fsync (a same-key move rotates the list; dst keeps its
/// TTL). Replies the element, null bulk when `src` is empty.
async fn move_elem(
    ctx: &mut Ctx<'_>,
    src: Vec<u8>,
    dst: Vec<u8>,
    pop_left: bool,
    push_left: bool,
    cmd: &str,
) {
    if !setops::same_slot(&[src.clone(), dst.clone()]) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    // Distinct latch keys in byte order (multi-key ABBA rule).
    let _guards = lock_sorted(ctx, &[src.clone(), dst.clone()]).await;

    let now = expire::now_ms();
    let (src_expire, src_meta) = match list_state(&ctx.shared.store, &ctx.prefix_key, &src, now) {
        ListState::List { expire_ms, meta } => (expire_ms, meta),
        ListState::Missing => {
            append_null(ctx.out);
            return;
        }
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    if src_meta.is_empty() {
        append_null(ctx.out);
        return;
    }
    let (dst_expire, dst_meta) = match list_state(&ctx.shared.store, &ctx.prefix_key, &dst, now) {
        ListState::List { expire_ms, meta } => (expire_ms, meta),
        ListState::Missing => (0, blank_meta()),
        ListState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let Ok((elem, src_after)) = pop_one(
        &ctx.shared.store,
        &ctx.prefix_key,
        &src,
        &src_meta,
        pop_left,
    ) else {
        append_error(ctx.out, &format!("ERR: {cmd} failed"));
        return;
    };
    let (pop_is_left, pop_idx) = if pop_left {
        list_ds::pop_left_target(&src_meta)
    } else {
        list_ds::pop_right_target(&src_meta)
    };
    let same = src == dst;
    // Same-key moves merge both halves into ONE meta (the later dst
    // write wins); src is written separately only for two-key moves.
    let mut batch = WriteBatch::default();
    if pop_is_left {
        list_ds::del_l(&mut batch, &ctx.prefix_key, &src, pop_idx);
    } else {
        list_ds::del_r(&mut batch, &ctx.prefix_key, &src, pop_idx);
    }
    if !same {
        if src_after.is_empty() {
            list_ds::delete_family(&mut batch, &ctx.prefix_key, &src, src_expire);
        } else {
            expire::set_ttl_entries(
                &mut batch,
                &ctx.prefix_key,
                list_ds::meta_key(&ctx.prefix_key, &src),
                src_expire,
                src_expire,
            );
            list_ds::write_meta(&mut batch, &ctx.prefix_key, &src, &src_after);
        }
    }
    let mut dst_after = if same { src_after } else { dst_meta };
    if push_left {
        list_ds::put_l(&mut batch, &ctx.prefix_key, &dst, dst_after.l_next, &elem);
        dst_after.l_next += 1;
        dst_after.l_count += 1;
    } else {
        list_ds::put_r(&mut batch, &ctx.prefix_key, &dst, dst_after.r_next, &elem);
        dst_after.r_next += 1;
        dst_after.r_count += 1;
    }
    expire::set_ttl_entries(
        &mut batch,
        &ctx.prefix_key,
        list_ds::meta_key(&ctx.prefix_key, &dst),
        dst_expire,
        dst_expire,
    );
    list_ds::write_meta(&mut batch, &ctx.prefix_key, &dst, &dst_after);
    if ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .is_ok()
    {
        wait::notify(
            &ctx.shared.wait_hub,
            &list_ds::meta_key(&ctx.prefix_key, &dst),
        );
        wait::notify(
            &ctx.shared.wait_hub,
            &list_ds::meta_key(&ctx.prefix_key, &src),
        );
        append_bulk(ctx.out, &elem);
    } else {
        append_error(ctx.out, &format!("ERR: {cmd} failed"));
    }
}

/// LMOVE src dst LEFT|RIGHT LEFT|RIGHT -> element (4 ctx.args; arity 5 counts the name).
pub async fn lmove(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        arity(ctx.out, "lmove");
        return;
    }
    let Some((pop_left, push_left)) = parse_dirs(&ctx.args[2], &ctx.args[3]) else {
        append_error(ctx.out, "ERR syntax error");
        return;
    };
    move_elem(
        ctx,
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        pop_left,
        push_left,
        "lmove",
    )
    .await;
}

/// RPOPLPUSH src dst = LMOVE src dst RIGHT LEFT (no timeout arg).
pub async fn rpoplpush(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "rpoplpush");
        return;
    }
    move_elem(
        ctx,
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        false,
        true,
        "rpoplpush",
    )
    .await;
}
