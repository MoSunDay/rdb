//! FT.BUILD, the SPANN trainer command: `FT.BUILD <index> [K <n>]
//! [ITERS <n>] [SEED <n>]` retrains the centroids + SQ8 calibration
//! and re-partitions every vector doc (text postings untouched) via
//! `ann::build_batch`, one batched fsync. Re-exported through
//! `ft_cmd` so the command table path stays put.

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::zset_util::eq_ignore_case;
use crate::command::{keys_core, Ctx};
use crate::ds::latch;
use crate::resp::codec::{append_error, append_string};

use super::ft_cmd::{index_state, IndexState};

/// `FT.BUILD <index> [K <n>] [ITERS <n>] [SEED <n>]` -> +OK; retrains
/// the SPANN centroids + SQ8 calibration and re-partitions every
/// vector doc (text postings untouched).
pub async fn ft_build(ctx: &mut Ctx<'_>) {
    let (mut k, mut iters, mut seed) = (32usize, 8usize, 0u64);
    let mut i = 1;
    while i < ctx.args.len() {
        let arg = ctx.args[i].as_slice();
        let bad = "ERR invalid ft.build option";
        let Some(val) = ctx.args.get(i + 1) else {
            arity(ctx.out, "ft.build");
            return;
        };
        let parsed = std::str::from_utf8(val)
            .ok()
            .and_then(|t| t.parse::<u64>().ok());
        if eq_ignore_case(arg, b"K") {
            match parsed {
                Some(v) if v > 0 => k = v.min(4096) as usize,
                _ => {
                    append_error(ctx.out, bad);
                    return;
                }
            }
        } else if eq_ignore_case(arg, b"ITERS") {
            match parsed {
                Some(v) if v > 0 => iters = v.min(1000) as usize,
                _ => {
                    append_error(ctx.out, bad);
                    return;
                }
            }
        } else if eq_ignore_case(arg, b"SEED") {
            match parsed {
                Some(v) => seed = v,
                _ => {
                    append_error(ctx.out, bad);
                    return;
                }
            }
        } else {
            append_error(ctx.out, bad);
            return;
        }
        i += 2;
    }
    let index = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &index),
    )
    .await;
    let meta = match index_state(&ctx.shared.store, &ctx.prefix_key, &index) {
        IndexState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        IndexState::Missing => {
            append_error(ctx.out, "ERR unknown index");
            return;
        }
        IndexState::Present(m) => m,
    };
    match super::ann::build_batch(
        &ctx.shared.store,
        &ctx.prefix_key,
        &index,
        &meta,
        k,
        iters,
        seed,
    ) {
        Ok(batch) => match ctx.commit(batch).await {
            Ok(()) => append_string(ctx.out, "OK"),
            Err(_) => append_error(ctx.out, "ERR: ft.build failed"),
        },
        Err(e) => append_error(ctx.out, &e),
    }
}
