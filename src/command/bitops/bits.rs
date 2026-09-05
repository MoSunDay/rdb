//! Pure bit/range math for the bitmap commands (Redis `src/bitops.c`):
//! argument parsing for BYTE|BIT ranges and MSB-first bit addressing.
//! Everything here is a free function over plain data — no `Ctx`, no
//! storage, no latches; the handlers in `bitops.rs` own those.

use crate::command::hash_cmd::parse_i64;
use crate::resp::codec::append_error;

/// Unparsable BITCOUNT/BITPOS range indexes.
pub(super) const NOT_INT_ERR: &str = "ERR value is not an integer or out of range";
/// Redis caps bitmaps at 512MB: bit 2^32-1 is the last addressable.
pub(super) const MAX_BIT_OFFSET: i64 = (1 << 32) - 1;

/// BYTE (default) vs BIT addressing of BITCOUNT/BITPOS range args.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum RangeUnit {
    Byte,
    Bit,
}

/// Parse a `[start [end [BYTE|BIT]]]` suffix: the BITPOS shape; BITCOUNT
/// calls it only with the 0/2/3-arg shapes (its 1-arg shape is rejected
/// by the caller first). Errors are replied to `out` before returning
/// `None`. Yields `(start, end, unit, end_given)`; `end == None` means
/// "to the string end".
pub(super) fn parse_range(
    out: &mut Vec<u8>,
    args: &[Vec<u8>],
) -> Option<(Option<i64>, Option<i64>, RangeUnit, bool)> {
    let (start, end, unit) = match args.len() {
        0 => (None, None, RangeUnit::Byte),
        1 => {
            let Some(s) = parse_i64(&args[0]) else {
                append_error(out, NOT_INT_ERR);
                return None;
            };
            (Some(s), None, RangeUnit::Byte)
        }
        2 | 3 => {
            let (Some(s), Some(e)) = (parse_i64(&args[0]), parse_i64(&args[1])) else {
                append_error(out, NOT_INT_ERR);
                return None;
            };
            let unit = if args.len() == 3 {
                parse_unit(out, &args[2])?
            } else {
                RangeUnit::Byte
            };
            (Some(s), Some(e), unit)
        }
        _ => {
            append_error(out, "ERR syntax error");
            return None;
        }
    };
    Some((start, end, unit, end.is_some()))
}

/// BYTE|BIT token (case-insensitive); anything else is a syntax error.
fn parse_unit(out: &mut Vec<u8>, arg: &[u8]) -> Option<RangeUnit> {
    match arg.to_ascii_uppercase().as_slice() {
        b"BYTE" => Some(RangeUnit::Byte),
        b"BIT" => Some(RangeUnit::Bit),
        _ => {
            append_error(out, "ERR syntax error");
            None
        }
    }
}

/// A 0..=2^32-1 bit offset (Redis's 512MB bitmap cap).
pub(super) fn parse_bit_offset(arg: &[u8]) -> Option<u64> {
    let n = parse_i64(arg)?;
    (0..=MAX_BIT_OFFSET).contains(&n).then_some(n as u64)
}

/// Bit `offset` of the (conceptually zero-padded) string `v`; offsets
/// past the end read as zero.
pub(super) fn bit_at(v: &[u8], offset: u64) -> u8 {
    match v.get((offset >> 3) as usize) {
        Some(byte) => (byte >> (7 - (offset & 7))) & 1,
        None => 0,
    }
}

/// Resolve an optional BYTE|BIT range to an inclusive `[lo, hi]` BIT
/// window over a `len`-byte string. Negative indexes count from the tail
/// (in the range's unit), `end == None` means "to the string end"; both
/// ends clamp to the string like Redis (`start` only from below). `None`
/// = empty window (empty string, or start > end after clamping).
pub(super) fn bit_window(
    len: usize,
    unit: RangeUnit,
    start: Option<i64>,
    end: Option<i64>,
) -> Option<(usize, usize)> {
    let totlen = match unit {
        RangeUnit::Bit => (len * 8) as i64,
        RangeUnit::Byte => len as i64,
    };
    if totlen == 0 {
        return None;
    }
    let (mut s, mut e) = (start.unwrap_or(0), end.unwrap_or(totlen - 1));
    if s < 0 {
        s += totlen;
    }
    if e < 0 {
        e += totlen;
    }
    if s < 0 {
        s = 0;
    }
    if e < 0 {
        e = 0;
    }
    if e >= totlen {
        e = totlen - 1;
    }
    if s > e {
        return None;
    }
    let (s, e) = (s as usize, e as usize);
    Some(match unit {
        RangeUnit::Byte => (s * 8, e * 8 + 7),
        RangeUnit::Bit => (s, e),
    })
}

/// Bits of byte `b` (MSB-first) covered by the window `[lo, hi]`: the
/// BIT-mode edges mask off out-of-range bits of the first/last byte,
/// exactly like Redis's first/last byte neg-masks.
fn window_mask(b: usize, lo: usize, hi: usize) -> u8 {
    let mut m = 0xffu8;
    if b == lo / 8 {
        m &= 0xff >> (lo % 8); // keep positions >= lo%8
    }
    if b == hi / 8 {
        m &= ((0xffu16 << (7 - hi % 8)) & 0xff) as u8; // keep <= hi%8
    }
    m
}

/// Set bits in the inclusive `[lo, hi]` bit window.
pub(super) fn popcount_window(v: &[u8], lo: usize, hi: usize) -> u64 {
    (lo / 8..=hi / 8)
        .map(|b| u64::from((v[b] & window_mask(b, lo, hi)).count_ones()))
        .sum()
}

/// First bit position in `[lo, hi]` whose value equals `bit`; `None`
/// when the window holds no such bit.
pub(super) fn find_bit(v: &[u8], lo: usize, hi: usize, bit: u8) -> Option<usize> {
    for b in lo / 8..=hi / 8 {
        let m = window_mask(b, lo, hi);
        let byte = v[b] & m;
        if (bit == 1 && byte != 0) || (bit == 0 && byte != m) {
            return (0..8)
                .map(|p| b * 8 + p)
                .find(|&p| bit_at(v, p as u64) == bit);
        }
    }
    None
}
