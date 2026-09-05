//! Multi-key set algebra: SUNION/SINTER/SDIFF (read) and their *STORE
//! twins, plus SINTERCARD (capped intersection cardinality). Cluster
//! rule: every key (destination included) must hash to the
//! same slot -- the RESP layer derives one prefix from the FIRST key only
//! -- else `-ERR CROSSSLOT ...` (Redis cluster wording; the Go rdb has no
//! multi-key sets, so Redis's text is the contract).
//!
//! STORE variants lock every distinct key's latch in byte order (ABBA
//! rule), read the sources BEFORE building the batch (a source may equal
//! the destination), then wipe + rebuild the destination and write meta +
//! members in ONE fsync. Results carry no TTL (Redis: SUNIONSTORE clears
//! any destination TTL).

use std::collections::HashSet;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::set_cmd::set_state;
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, setops};
use crate::resp::codec::{append_array, append_bulk, append_error, append_int};
use setops::require_same_slot;

/// Which algebra the shared bodies compute.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    Union,
    Inter,
    Diff,
}

impl Op {
    fn apply(self, sets: &[HashSet<Vec<u8>>]) -> Vec<Vec<u8>> {
        match self {
            Op::Union => setops::union_all(sets),
            Op::Inter => setops::intersect_all(sets),
            Op::Diff => setops::diff_all(sets),
        }
    }
}

/// Read all operands into member sets; `Err(())` already replied (wrong
/// type). Missing keys read as empty sets (Redis semantics).
fn operand_sets(
    ctx: &mut Ctx<'_>,
    keys: &[Vec<u8>],
    now: u64,
) -> Result<Vec<HashSet<Vec<u8>>>, ()> {
    let mut sets = Vec::with_capacity(keys.len());
    for key in keys {
        match setops::read_members(&ctx.shared.store, &ctx.prefix_key, key, now) {
            setops::MembersRead::Members(m) => sets.push(m),
            setops::MembersRead::Missing => sets.push(HashSet::new()),
            setops::MembersRead::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return Err(());
            }
            setops::MembersRead::Failed(_) => {
                append_error(ctx.out, "ERR: read failed");
                return Err(());
            }
        }
    }
    Ok(sets)
}

/// Shared body of SUNION/SINTER/SDIFF.
async fn read_variant(ctx: &mut Ctx<'_>, cmd: &str, op: Op) {
    if ctx.args.is_empty() {
        arity(ctx.out, cmd);
        return;
    }
    if !setops::same_slot(&ctx.args) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    let keys = ctx.args.clone();
    let now = expire::now_ms();
    let Ok(sets) = operand_sets(ctx, &keys, now) else {
        return;
    };
    let result = op.apply(&sets);
    append_array(ctx.out, result.len());
    for m in &result {
        append_bulk(ctx.out, m);
    }
}

/// Shared body of SUNIONSTORE/SINTERSTORE/SDIFFSTORE: overwrite the
/// destination with the algebra over the sources; reply the new size.
async fn store_variant(ctx: &mut Ctx<'_>, cmd: &str, op: Op) {
    if ctx.args.len() < 2 {
        arity(ctx.out, cmd);
        return;
    }
    if !setops::same_slot(&ctx.args) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    // Distinct latch keys in byte order (multi-key ABBA rule).
    let mut latches: Vec<Vec<u8>> = ctx
        .args
        .iter()
        .map(|k| keys_core::latch_key(&ctx.prefix_key, k))
        .collect();
    latches.sort();
    latches.dedup();
    let mut guards = Vec::with_capacity(latches.len());
    for k in &latches {
        guards.push(latch::lock(&ctx.shared.latch, k).await);
    }
    let _guards = guards;

    let dst = ctx.args[0].clone();
    let sources = ctx.args[1..].to_vec();
    let now = expire::now_ms();
    let Ok(sets) = operand_sets(ctx, &sources, now) else {
        return;
    };
    let result = op.apply(&sets);
    // Redis: a destination holding a non-set value errors WRONGTYPE
    // (only set-or-missing destinations may be overwritten).
    if let crate::command::set_cmd::SetState::WrongType = set_state(ctx, &dst) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let mut batch = WriteBatch::default();
    // Empty result deletes the destination (an empty set does not exist).
    let card = if result.is_empty() {
        setops::store_clear(&mut batch, &ctx.shared.store, &ctx.prefix_key, &dst, now);
        0
    } else {
        setops::store_set(
            &mut batch,
            &ctx.shared.store,
            &ctx.prefix_key,
            &dst,
            &result,
            now,
        )
    };
    if ctx.commit(batch).await.is_ok() {
        append_int(ctx.out, card as i64);
    } else {
        append_error(ctx.out, &format!("ERR: {cmd} failed"));
    }
}

pub async fn sunion(ctx: &mut Ctx<'_>) {
    read_variant(ctx, "sunion", Op::Union).await;
}

pub async fn sinter(ctx: &mut Ctx<'_>) {
    read_variant(ctx, "sinter", Op::Inter).await;
}

pub async fn sdiff(ctx: &mut Ctx<'_>) {
    read_variant(ctx, "sdiff", Op::Diff).await;
}

pub async fn sunionstore(ctx: &mut Ctx<'_>) {
    store_variant(ctx, "sunionstore", Op::Union).await;
}

pub async fn sinterstore(ctx: &mut Ctx<'_>) {
    store_variant(ctx, "sinterstore", Op::Inter).await;
}

pub async fn sdiffstore(ctx: &mut Ctx<'_>) {
    store_variant(ctx, "sdiffstore", Op::Diff).await;
}

/// SINTERCARD numkeys key [key ...] [LIMIT limit] -> cardinality of the
/// intersection (missing keys = empty sets). A pure read: no latch.
/// `numkeys` must be a positive integer that matches the number of key
/// arguments that follow; LIMIT caps the computation early -- once the
/// running count reaches it the answer is `limit` (LIMIT 0 = unlimited).
pub async fn sintercard(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "sintercard");
        return;
    }
    let numkeys = match parse_i64(&ctx.args[0]) {
        Some(n) if n > 0 => n,
        _ => {
            append_error(ctx.out, "ERR numkeys should be greater than 0");
            return;
        }
    };
    if numkeys as usize > ctx.args.len() - 1 {
        append_error(
            ctx.out,
            "ERR Number of keys can't be greater than number of args",
        );
        return;
    }
    let keys = ctx.args[1..1 + numkeys as usize].to_vec();
    if !require_same_slot(ctx.out, &keys) {
        return;
    }
    let mut limit: i64 = -1; // -1 / 0 both mean unlimited
    let mut i = 1 + numkeys as usize;
    while i < ctx.args.len() {
        if ctx.args[i].eq_ignore_ascii_case(b"LIMIT") && i + 1 < ctx.args.len() {
            match parse_i64(&ctx.args[i + 1]) {
                Some(n) if n >= 0 => limit = n,
                _ => {
                    append_error(ctx.out, "ERR value is not an integer or out of range");
                    return;
                }
            }
            i += 2;
        } else {
            // Any other token, or a dangling LIMIT, is a syntax error.
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    }
    let now = expire::now_ms();
    let Ok(sets) = operand_sets(ctx, &keys, now) else {
        return;
    };
    // Count over the smallest operand (the intersection can never be
    // bigger), stopping as soon as LIMIT is provably reached.
    let Some(seed) = sets.iter().min_by_key(|s| s.len()) else {
        append_int(ctx.out, 0);
        return;
    };
    let cap = usize::try_from(limit).unwrap_or(0);
    let mut card = 0usize;
    for m in seed {
        if sets.iter().all(|s| s.contains(m)) {
            card += 1;
            if limit > 0 && card >= cap {
                break;
            }
        }
    }
    append_int(ctx.out, card as i64);
}
