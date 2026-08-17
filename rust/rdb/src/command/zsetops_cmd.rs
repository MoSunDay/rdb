//! Multi-key sorted-set algebra: ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE
//! (`<cmd> dest numkeys key [key...] [WEIGHTS w...] [AGGREGATE
//! SUM|MIN|MAX]`, options in any order after the keys). Sources are
//! read fully into memory (member -> per-source scores), combined by
//! membership rule (any / all / first-only), scored by the aggregate
//! over WEIGHTS-scaled per-source scores, then the destination family
//! is wiped and rebuilt in ONE fsync -- the destination TTL is NOT
//! carried over (Redis: a STORE clears any destination TTL).
//!
//! Cluster rule mirrors `setops_cmd`: destination included, every key
//! must hash to the same slot, else `-ERR CROSSSLOT ...`; STORE
//! variants lock every distinct key's latch in byte order (ABBA rule).

use std::collections::{HashMap, HashSet};

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_f64, parse_i64, WRONGTYPE};
use crate::command::zset_util::{collect_scored, commit_zset, zset_state, ZSetState};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, setops, wait, zset_ds};
use crate::resp::codec::{append_error, append_int};

/// Which membership rule the combination computes.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    Union,
    Inter,
    Diff,
}

/// How per-source weighted scores fold into one result score.
#[derive(Clone, Copy, PartialEq)]
enum Agg {
    Sum,
    Min,
    Max,
}

/// A fully parsed STORE command line: destination, sources, weights
/// (one per source) and the fold.
struct StoreOpts {
    dest: Vec<u8>,
    keys: Vec<Vec<u8>>,
    weights: Vec<f64>,
    agg: Agg,
}

/// Parse `dest numkeys key... [opts...]`; every failure replies its
/// Redis error text and answers `None`.
fn parse_store_opts(out: &mut Vec<u8>, args: &[Vec<u8>], cmd: &str) -> Option<StoreOpts> {
    if args.len() < 2 {
        arity(out, cmd);
        return None;
    }
    let numkeys = match parse_i64(&args[1]) {
        Some(n) if n >= 1 => n as usize,
        Some(_) => {
            append_error(
                out,
                &format!("ERR at least 1 input key is needed for '{cmd}' command"),
            );
            return None;
        }
        None => {
            append_error(out, "ERR value is not an integer or out of range");
            return None;
        }
    };
    if args.len() < 2 + numkeys {
        append_error(out, "ERR syntax error, wrong number of keys");
        return None;
    }
    let dest = args[0].clone();
    let keys = args[2..2 + numkeys].to_vec();
    let mut weights = vec![1.0; numkeys];
    let mut agg = Agg::Sum;
    let mut i = 2 + numkeys;
    while i < args.len() {
        let opt = args[i].as_slice();
        let upper = |kw: &[u8]| crate::command::zset_util::eq_ignore_case(opt, kw);
        if upper(b"WEIGHTS") {
            if i + numkeys >= args.len() {
                append_error(out, "ERR WEIGHTS options doesn't match the number of keys");
                return None;
            }
            for (j, w) in args[i + 1..i + 1 + numkeys].iter().enumerate() {
                match parse_f64(w) {
                    // Finite or +-inf; NaN would poison the sort order.
                    Some(v) if !v.is_nan() => weights[j] = v,
                    _ => {
                        append_error(out, "ERR weight value is not a float");
                        return None;
                    }
                }
            }
            i += 1 + numkeys;
        } else if upper(b"AGGREGATE") {
            let Some(a) = args.get(i + 1) else {
                append_error(out, "ERR syntax error");
                return None;
            };
            agg = if crate::command::zset_util::eq_ignore_case(a, b"SUM") {
                Agg::Sum
            } else if crate::command::zset_util::eq_ignore_case(a, b"MIN") {
                Agg::Min
            } else if crate::command::zset_util::eq_ignore_case(a, b"MAX") {
                Agg::Max
            } else {
                append_error(out, "ERR syntax error");
                return None;
            };
            i += 2;
        } else {
            append_error(out, "ERR syntax error");
            return None;
        }
    }
    Some(StoreOpts {
        dest,
        keys,
        weights,
        agg,
    })
}

/// Read one source into a member -> score map; `Err(())` already
/// replied (wrong type). Missing keys read as empty zsets.
fn source_map(ctx: &mut Ctx<'_>, key: &[u8], now: u64) -> Result<HashMap<Vec<u8>, f64>, ()> {
    match zset_state(&ctx.shared.store, &ctx.prefix_key, key, now) {
        ZSetState::ZSet { .. } => Ok(collect_scored(&ctx.shared.store, &ctx.prefix_key, key)
            .into_iter()
            .collect()),
        ZSetState::Missing => Ok(HashMap::new()),
        ZSetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            Err(())
        }
    }
}

/// Which members survive: UNION keeps any, INTER keeps those in every
/// source, DIFF keeps source 0's exclusives.
fn op_members(op: Op, maps: &[HashMap<Vec<u8>, f64>]) -> Vec<Vec<u8>> {
    match op {
        Op::Union => {
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            let mut out: Vec<Vec<u8>> = Vec::new();
            for map in maps {
                for member in map.keys() {
                    if seen.insert(member.clone()) {
                        out.push(member.clone());
                    }
                }
            }
            out
        }
        Op::Inter => maps
            .first()
            .map(|first| {
                first
                    .keys()
                    .filter(|m| maps.iter().all(|s| s.contains_key(*m)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        Op::Diff => maps
            .first()
            .map(|first| {
                first
                    .keys()
                    .filter(|m| maps[1..].iter().all(|s| !s.contains_key(*m)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Fold the weighted per-source scores of one member; `None` when the
/// result is not a number (inf * 0, +inf + -inf under SUM, ...).
fn aggregate(
    maps: &[HashMap<Vec<u8>, f64>],
    weights: &[f64],
    agg: Agg,
    member: &[u8],
) -> Option<f64> {
    let mut acc: Option<f64> = None;
    for (i, map) in maps.iter().enumerate() {
        if let Some(score) = map.get(member) {
            let weighted = score * weights[i];
            if weighted.is_nan() {
                return None;
            }
            acc = Some(match (acc, agg) {
                (None, _) => weighted,
                (Some(a), Agg::Sum) => a + weighted,
                (Some(a), Agg::Min) => a.min(weighted),
                (Some(a), Agg::Max) => a.max(weighted),
            });
        }
    }
    acc.filter(|s| !s.is_nan())
}

/// Shared body: parse, latch, read sources, combine, rebuild dest.
async fn store_variant(ctx: &mut Ctx<'_>, cmd: &str, op: Op) {
    let Some(opts) = parse_store_opts(ctx.out, &ctx.args, cmd) else {
        return;
    };
    let mut all = Vec::with_capacity(opts.keys.len() + 1);
    all.push(opts.dest.clone());
    all.extend(opts.keys.iter().cloned());
    if !setops::same_slot(&all) {
        append_error(ctx.out, setops::CROSSSLOT_ERROR);
        return;
    }
    // Distinct latch keys in byte order (multi-key ABBA rule).
    let mut latches: Vec<Vec<u8>> = all
        .iter()
        .map(|k| keys_core::latch_key(&ctx.prefix_key, k))
        .collect();
    latches.sort();
    latches.dedup();
    let _guards: Vec<_> = latches
        .iter()
        .map(|k| latch::lock(&ctx.shared.latch, k))
        .collect();

    let now = expire::now_ms();
    let mut maps: Vec<HashMap<Vec<u8>, f64>> = Vec::with_capacity(opts.keys.len());
    for key in &opts.keys {
        match source_map(ctx, key, now) {
            Ok(m) => maps.push(m),
            Err(()) => return,
        }
    }
    let mut result: Vec<(Vec<u8>, f64)> = Vec::new();
    for member in op_members(op, &maps) {
        match aggregate(&maps, &opts.weights, opts.agg, &member) {
            Some(score) => result.push((member, score)),
            None => {
                append_error(ctx.out, "ERR resulting score is not a number");
                return;
            }
        }
    }
    // Destination may only be overwritten when absent or a zset.
    let dest_expire = match zset_state(&ctx.shared.store, &ctx.prefix_key, &opts.dest, now) {
        ZSetState::ZSet { expire_ms, .. } => expire_ms,
        ZSetState::Missing => 0,
        ZSetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    let mut batch = WriteBatch::default();
    // Wipe whatever the destination held (family + TTL entry), then
    // rebuild; commit_zset writes the fresh no-TTL meta (or wipes the
    // family again when the result is empty).
    zset_ds::delete_family(&mut batch, &ctx.prefix_key, &opts.dest, dest_expire);
    for (member, score) in &result {
        zset_ds::put_member(&mut batch, &ctx.prefix_key, &opts.dest, member, *score);
        zset_ds::put_scored(&mut batch, &ctx.prefix_key, &opts.dest, *score, member);
    }
    if !commit_zset(ctx, &opts.dest, 0, result.len() as u64, batch, cmd).await {
        return;
    }
    if !result.is_empty() {
        wait::notify(
            &ctx.shared.wait_hub,
            &zset_ds::meta_key(&ctx.prefix_key, &opts.dest),
        );
    }
    append_int(ctx.out, result.len() as i64);
}

pub async fn zunionstore(ctx: &mut Ctx<'_>) {
    store_variant(ctx, "zunionstore", Op::Union).await;
}

pub async fn zinterstore(ctx: &mut Ctx<'_>) {
    store_variant(ctx, "zinterstore", Op::Inter).await;
}

pub async fn zdiffstore(ctx: &mut Ctx<'_>) {
    store_variant(ctx, "zdiffstore", Op::Diff).await;
}
