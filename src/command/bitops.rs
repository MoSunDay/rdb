//! Bitmap commands (Redis `src/bitops.c`): SETBIT, GETBIT, BITCOUNT,
//! BITPOS, BITOP — semantics verified against a live Redis 7.2.5 and the
//! Redis 7.2.5/8.0.3 sources. Notable contracts:
//!
//! * Bit offset n addresses byte `n/8` MSB-first: `SETBIT k 0 1` stores
//!   `\x80`, `SETBIT k 7 1` stores `\x01`. Offsets are capped at 2^32-1
//!   bits (Redis's 512MB proto-max-bulk), so SETBIT materializes at most
//!   512MB, `offset/8 + 1` bytes for a given offset.
//! * SETBIT (Redis >= 7.0) zero-extends the string even when clearing a
//!   bit and creates missing keys; the rewrite keeps the old TTL.
//! * BITCOUNT/BITPOS ranges: BYTE default, BIT addresses bit positions;
//!   signed indexes, negative from the tail, inclusive ends, clamped;
//!   inverted (start > end) -> 0 / -1. BITPOS also accepts start-only.
//! * BITPOS: bit=1 never found -> -1; bit=0 counts the region right of
//!   the string as zero-padded when no end was given (all-ones string ->
//!   total bit count), while an explicit end confines the window (all
//!   ones -> -1). Missing keys short-circuit to 0/-1 before range
//!   parsing; existing empty strings reply -1. Real Redis's bad-bit
//!   error is "ERR The bit argument must be 1 or 0." (kept verbatim).
//! * BITOP: NOT takes one source; AND/OR/XOR are element-wise over the
//!   longest source (shorter ones read as zero bytes); the destination
//!   is overwritten whatever kind it held, TTL dropped (WRONGTYPE
//!   applies to sources only); an empty result deletes the destination.
//!   Destination and sources must share one slot (CROSSSLOT otherwise).

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{self, parse_i64};
use crate::command::keys_core;
use crate::command::string::{
    arity, clear_key_family, old_string_value, write_string_record, OldValue,
};
use crate::command::Ctx;
use crate::ds::{expire, latch, setops};
use crate::resp::codec::{append_error, append_int};

#[path = "bitops/bits.rs"]
mod bits;

use bits::{bit_at, bit_window, find_bit, parse_bit_offset, parse_range, popcount_window};

/// SETBIT/GETBIT offset errors share this wording (Redis).
const OFFSET_ERR: &str = "ERR bit offset is not an integer or out of range";
/// SETBIT's value must be exactly 0 or 1.
const BIT_ERR: &str = "ERR bit is not an integer or out of range";
/// BITPOS's bit argument keeps Redis's own (still current) wording.
const BITPOS_BIT_ERR: &str = "ERR The bit argument must be 1 or 0.";
/// Unparsable BITCOUNT/BITPOS range indexes.
const NOT_INT_ERR: &str = bits::NOT_INT_ERR;

/// `SETBIT key offset value` -> the previous bit at `offset`.
///
/// The whole (zero-padded) value is rewritten under the key's previous
/// deadline, so sparse offsets materialize `offset/8 + 1` stored bytes
/// exactly like Redis does.
pub async fn setbit(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "setbit");
        return;
    }
    // Offset first, then the value: Redis reports them in this order.
    let Some(offset) = parse_bit_offset(&ctx.args[1]) else {
        append_error(ctx.out, OFFSET_ERR);
        return;
    };
    let Some(on) = parse_i64(&ctx.args[2]) else {
        append_error(ctx.out, BIT_ERR);
        return;
    };
    if on & !1 != 0 {
        append_error(ctx.out, BIT_ERR);
        return;
    }
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &key, now);
    let mut value = match old_string_value(&state) {
        OldValue::Str(v) => v,
        OldValue::Missing => Vec::new(),
        OldValue::WrongType => {
            append_error(ctx.out, hash_cmd::WRONGTYPE);
            return;
        }
    };
    let old = bit_at(&value, offset);
    let byte = (offset >> 3) as usize;
    if value.len() <= byte {
        value.resize(byte + 1, 0);
    }
    let mask = 1u8 << (7 - (offset & 7));
    value[byte] = if on == 1 {
        value[byte] | mask
    } else {
        value[byte] & !mask
    };
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &key, &state);
    write_string_record(&mut batch, &ctx.prefix_key, &key, &value, state.expire_ms());
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, i64::from(old)),
        Err(_) => append_error(ctx.out, "ERR: setbit failed"),
    }
}

/// `GETBIT key offset` -> :0/:1; missing keys and out-of-string offsets
/// read as zero bits.
pub async fn getbit(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "getbit");
        return;
    }
    let Some(offset) = parse_bit_offset(&ctx.args[1]) else {
        append_error(ctx.out, OFFSET_ERR);
        return;
    };
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    match old_string_value(&state) {
        OldValue::Str(v) => append_int(ctx.out, i64::from(bit_at(&v, offset))),
        OldValue::Missing => append_int(ctx.out, 0),
        OldValue::WrongType => append_error(ctx.out, hash_cmd::WRONGTYPE),
    }
}

/// `BITCOUNT key [start end [BYTE|BIT]]` -> set bits in the window.
pub async fn bitcount(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        arity(ctx.out, "bitcount");
        return;
    }
    // Key lookup precedes range parsing: missing keys reply 0 and wrong
    // kinds reply WRONGTYPE even when the range args are garbage.
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    let value = match old_string_value(&state) {
        OldValue::Str(v) => v,
        OldValue::Missing => {
            append_int(ctx.out, 0);
            return;
        }
        OldValue::WrongType => {
            append_error(ctx.out, hash_cmd::WRONGTYPE);
            return;
        }
    };
    let args = ctx.args[1..].to_vec();
    // BITCOUNT rejects the start-only form BITPOS accepts (Redis has no
    // 3-argc shape for it).
    if args.len() == 1 {
        append_error(ctx.out, "ERR syntax error");
        return;
    }
    let Some((start, end, unit, _)) = parse_range(ctx.out, &args) else {
        return;
    };
    let bits = match bit_window(value.len(), unit, start, end) {
        Some((lo, hi)) => popcount_window(&value, lo, hi),
        None => 0,
    };
    append_int(ctx.out, bits as i64);
}

/// `BITPOS key bit [start [end [BYTE|BIT]]]` -> first bit equal to `bit`.
pub async fn bitpos(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "bitpos");
        return;
    }
    let Some(bit) = parse_i64(&ctx.args[1]) else {
        append_error(ctx.out, NOT_INT_ERR);
        return;
    };
    if bit & !1 != 0 {
        append_error(ctx.out, BITPOS_BIT_ERR);
        return;
    }
    let now = expire::now_ms();
    let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &ctx.args[0], now);
    let value = match old_string_value(&state) {
        OldValue::Str(v) => v,
        // Missing = an infinite array of zeros, even for a bogus range:
        // Redis replies before parsing any start/end argument.
        OldValue::Missing => {
            append_int(ctx.out, if bit == 0 { 0 } else { -1 });
            return;
        }
        OldValue::WrongType => {
            append_error(ctx.out, hash_cmd::WRONGTYPE);
            return;
        }
    };
    let args = ctx.args[2..].to_vec();
    let Some((start, end, unit, end_given)) = parse_range(ctx.out, &args) else {
        return;
    };
    let Some((lo, hi)) = bit_window(value.len(), unit, start, end) else {
        append_int(ctx.out, -1);
        return;
    };
    if let Some(pos) = find_bit(&value, lo, hi, bit as u8) {
        append_int(ctx.out, pos as i64);
    } else if bit == 1 || end_given {
        // Ones were exhausted, or the explicit end forbids reading past
        // the window as zero padding.
        append_int(ctx.out, -1);
    } else {
        // No end given: the first bit after the string is clear by
        // definition — the total bit count (empty strings resolve to an
        // empty window above and never reach here).
        append_int(ctx.out, (hi + 1) as i64);
    }
}

/// `BITOP and|or|xor|not destkey srckey...` -> result length in bytes.
pub async fn bitop(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        arity(ctx.out, "bitop");
        return;
    }
    let op = match ctx.args[0].to_ascii_lowercase().as_slice() {
        b"and" => Op::And,
        b"or" => Op::Or,
        b"xor" => Op::Xor,
        b"not" => Op::Not,
        _ => {
            append_error(ctx.out, "ERR syntax error");
            return;
        }
    };
    if op == Op::Not && ctx.args.len() != 3 {
        append_error(
            ctx.out,
            "ERR BITOP NOT must be called with a single source key.",
        );
        return;
    }
    if !setops::require_same_slot(ctx.out, &ctx.args[1..]) {
        return;
    }
    // Every distinct key (destination included) under its latch, in byte
    // order — the multi-key ABBA rule shared with the *STORE commands.
    let mut latches: Vec<Vec<u8>> = ctx.args[1..]
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

    let dst = ctx.args[1].clone();
    let sources = ctx.args[2..].to_vec();
    let now = expire::now_ms();
    let mut srcs = Vec::with_capacity(sources.len());
    for key in &sources {
        let state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, key, now);
        match old_string_value(&state) {
            OldValue::Str(v) => srcs.push(v),
            OldValue::Missing => srcs.push(Vec::new()),
            OldValue::WrongType => {
                append_error(ctx.out, hash_cmd::WRONGTYPE);
                return;
            }
        }
    }
    let result = op.apply(&srcs);
    // The destination keeps whatever kind it held — Redis's setKey
    // overwrites unconditionally — but its TTL is dropped, and an empty
    // result means the destination ceases to exist.
    let dst_state = keys_core::resolve_arc(&ctx.shared.store, &ctx.prefix_key, &dst, now);
    let mut batch = WriteBatch::default();
    clear_key_family(&mut batch, &ctx.prefix_key, &dst, &dst_state);
    if !result.is_empty() {
        write_string_record(&mut batch, &ctx.prefix_key, &dst, &result, 0);
    }
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, result.len() as i64),
        Err(_) => append_error(ctx.out, "ERR: bitop failed"),
    }
}

/// BITOP algebra.
#[derive(Clone, Copy, PartialEq)]
enum Op {
    And,
    Or,
    Xor,
    Not,
}

impl Op {
    /// Element-wise combine; shorter sources read as zero bytes (Redis
    /// pads with zeros, never truncates to the minimum length).
    fn apply(self, srcs: &[Vec<u8>]) -> Vec<u8> {
        if self == Op::Not {
            return srcs[0].iter().map(|b| !b).collect();
        }
        let maxlen = srcs.iter().map(Vec::len).max().unwrap_or(0);
        (0..maxlen)
            .map(|j| {
                let mut acc = if self == Op::And { 0xff } else { 0 };
                for s in srcs {
                    let byte = s.get(j).copied().unwrap_or(0);
                    acc = match self {
                        Op::And => acc & byte,
                        Op::Or => acc | byte,
                        Op::Xor => acc ^ byte,
                        Op::Not => unreachable!("handled above"),
                    };
                }
                acc
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
