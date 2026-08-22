//! LPOP/RPOP: one element (null bulk when missing) or a counted batch
//! off either end; a list drained to empty is deleted. Same read-scan ->
//! batch-build -> ONE-fsync shape as `list_cmd::commit_list`.

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::list_cmd::{commit_list, list_state, pop_one, ListState};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, list_ds};
use crate::resp::codec::{append_array, append_bulk, append_error, append_null};
/// when missing); with a positive count an array of up to `count`
/// elements, popped end first. `0` = empty array, negatives error.
async fn pop(ctx: &mut Ctx<'_>, left: bool, cmd: &str) {
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
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let (expire_ms, meta) =
        match list_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
            ListState::List { expire_ms, meta } => (expire_ms, meta),
            ListState::Missing => {
                reply_pop_empty(ctx, count);
                return;
            }
            ListState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
        };
    let len = meta.len();
    if len == 0 || count == Some(0) {
        reply_pop_empty(ctx, count);
        return;
    }
    let n = match count {
        None => 1,
        Some(c) => (c as u64).min(len),
    };
    let elems = if n == 1 {
        match pop_one(&ctx.shared.store, &ctx.prefix_key, &key, &meta, left) {
            Ok((elem, _)) => vec![elem],
            Err(_) => {
                append_error(ctx.out, &format!("ERR: {cmd} failed"));
                return;
            }
        }
    } else {
        let window = if left { (0, n - 1) } else { (len - n, len - 1) };
        match list_ds::collect_range(
            &ctx.shared.store,
            &ctx.prefix_key,
            &key,
            &meta,
            window.0,
            window.1,
        ) {
            Ok(mut v) => {
                if !left {
                    v.reverse();
                }
                v
            }
            Err(_) => {
                append_error(ctx.out, &format!("ERR: {cmd} failed"));
                return;
            }
        }
    };
    let mut batch = WriteBatch::default();
    let mut after = meta;
    let (take_l, take_r) = if left {
        let tl = n.min(meta.l_count);
        (tl, n - tl)
    } else {
        let tr = n.min(meta.r_count);
        (n - tr, tr)
    };
    if left {
        // L entries come off the top (l_next-1 down), then R off the
        // bottom (r_base up): r_next stays, r_base moves up.
        for i in 0..take_l {
            list_ds::del_l(&mut batch, &ctx.prefix_key, &key, meta.l_next - 1 - i);
        }
        for i in 0..take_r {
            list_ds::del_r(&mut batch, &ctx.prefix_key, &key, meta.r_base() + i);
        }
        after.l_next -= take_l;
        after.l_count -= take_l;
        after.r_count -= take_r;
    } else {
        // R entries come off the top (r_next-1 down), then L off the
        // bottom (l_base up): l_next stays, l_base moves up.
        for i in 0..take_r {
            list_ds::del_r(&mut batch, &ctx.prefix_key, &key, meta.r_next - 1 - i);
        }
        for i in 0..take_l {
            list_ds::del_l(&mut batch, &ctx.prefix_key, &key, meta.l_base() + i);
        }
        after.r_next -= take_r;
        after.r_count -= take_r;
        after.l_count -= take_l;
    }
    let emptied = after.is_empty();
    if commit_list(
        ctx,
        &key,
        expire_ms,
        if emptied { None } else { Some(&after) },
        batch,
        cmd,
    )
    .await
    {
        match count {
            None => append_bulk(ctx.out, &elems[0]),
            Some(_) => {
                append_array(ctx.out, elems.len());
                for e in &elems {
                    append_bulk(ctx.out, e);
                }
            }
        }
    }
}

fn reply_pop_empty(ctx: &mut Ctx<'_>, count: Option<i64>) {
    match count {
        None => append_null(ctx.out),
        Some(_) => append_array(ctx.out, 0),
    }
}

/// LPOP key [count].
pub async fn lpop(ctx: &mut Ctx<'_>) {
    pop(ctx, true, "lpop").await;
}

/// RPOP key [count].
pub async fn rpop(ctx: &mut Ctx<'_>) {
    pop(ctx, false, "rpop").await;
}
