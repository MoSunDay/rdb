//! Sorted-set shared plumbing: key-state resolution, score parsing and
//! formatting, and the single-fsync commit that every zset write funnels
//! through. Kept apart from the command handlers so the read/pop/range
//! modules import one focused module instead of reaching into ZADD's.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::WRONGTYPE;
use crate::command::{keys_core, Ctx};
use crate::ds::codec::{self, KIND_ZSET_META};
use crate::ds::{expire, zset_ds};
use crate::resp::codec::{append_bulk, append_error};
use crate::store::{ops, Store};

/// What one key is from the sorted-set commands' point of view.
#[derive(Debug, PartialEq)]
pub(crate) enum ZSetState {
    Missing,
    WrongType,
    ZSet { expire_ms: u64, count: u64 },
}

/// Resolve via keys_core: raw strings and foreign kinds -> WrongType; an
/// expired zset purges and reads as Missing.
pub(crate) fn zset_state(store: &Store, prefix: &[u8], key: &[u8], now: u64) -> ZSetState {
    match keys_core::resolve(store, prefix, key, now) {
        keys_core::KeyState::Missing => ZSetState::Missing,
        keys_core::KeyState::RawString { .. } => ZSetState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_ZSET_META => {
            ZSetState::WrongType
        }
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => ZSetState::ZSet {
            expire_ms,
            count: codec::decode_count(&payload),
        },
    }
}

/// Meta for a write path: `(expire_ms, count)`, replying WRONGTYPE and
/// answering `None` when the key holds another type.
pub(crate) fn write_meta_of(ctx: &mut Ctx<'_>, key: &[u8]) -> Option<(u64, u64)> {
    match zset_state(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        ZSetState::ZSet { expire_ms, count } => Some((expire_ms, count)),
        ZSetState::Missing => Some((0, 0)),
        ZSetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            None
        }
    }
}

/// One score argument: the infinity spellings, or any finite f64 (NaN is
/// rejected -- it has no place in the sortable order).
pub(crate) fn parse_score(s: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(s).ok()?;
    match text.to_ascii_lowercase().as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => return Some(f64::INFINITY),
        "-inf" | "-infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    let v: f64 = text.parse().ok()?;
    v.is_finite().then_some(v)
}

/// One range endpoint: a leading `(` makes the bound exclusive.
pub(crate) fn parse_score_bound(s: &[u8]) -> Option<(f64, bool)> {
    match s.strip_prefix(b"(") {
        Some(rest) => parse_score(rest).map(|score| (score, false)),
        None => parse_score(s).map(|score| (score, true)),
    }
}

/// Append one score as a bulk string: f64's shortest roundtrip repr
/// (`3.5`, `inf`, ...); Redis prints %.17g, both roundtrip to the same
/// value (see `hash_incr::hincrbyfloat`).
pub(crate) fn append_score(out: &mut Vec<u8>, score: f64) {
    append_bulk(out, format!("{score}").as_bytes());
}

/// Case-insensitive ASCII equality against a fixed keyword.
pub(crate) fn eq_ignore_case(arg: &[u8], keyword: &[u8]) -> bool {
    arg.len() == keyword.len()
        && arg
            .iter()
            .zip(keyword)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Commit one zset mutation: the record ops already sit in `batch`; this
/// appends the meta record (or a whole-family wipe once the count hits
/// zero -- empty zsets do not exist) and lands everything in ONE fsync.
/// `true` on success; the error reply is written here on failure.
pub(crate) async fn commit_zset(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    count: u64,
    batch: WriteBatch,
    cmd: &str,
) -> bool {
    let mut batch = batch;
    if count == 0 {
        zset_ds::delete_family(&mut batch, &ctx.prefix_key, key, expire_ms);
    } else {
        zset_ds::write_meta(
            &mut batch,
            &ctx.prefix_key,
            key,
            &zset_ds::ZSetMeta { expire_ms, count },
        );
    }
    ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .map(|_| true)
        .unwrap_or_else(|_| {
            append_error(ctx.out, &format!("ERR: {cmd} failed"));
            false
        })
}

/// Collect every member with its score in ascending order (the whole
/// score window); the base for rank/lex/random reads.
pub(crate) fn collect_scored(store: &Store, prefix: &[u8], key: &[u8]) -> Vec<(Vec<u8>, f64)> {
    let mut items = Vec::new();
    let _ = zset_ds::for_each_scored(store, prefix, key, b"", false, &mut |member, score| {
        items.push((member.to_vec(), score));
        true
    });
    items
}

/// One ZRANGEBYLEX endpoint: `-`/`+` are the infinite ends, `[x` and
/// `(x` include/exclude the member bytes `x`.
pub(crate) enum LexBound {
    NegInf,
    PosInf,
    Incl(Vec<u8>),
    Excl(Vec<u8>),
}

/// Parse one lex bound; `None` on anything but `-`/`+`/`[x`/`(x`.
pub(crate) fn parse_lex_bound(s: &[u8]) -> Option<LexBound> {
    if s == b"-" {
        return Some(LexBound::NegInf);
    }
    if s == b"+" {
        return Some(LexBound::PosInf);
    }
    match s.split_first() {
        Some((b'[', rest)) => Some(LexBound::Incl(rest.to_vec())),
        Some((b'(', rest)) => Some(LexBound::Excl(rest.to_vec())),
        _ => None,
    }
}

/// Bytewise membership test of `member` against the lex bounds.
pub(crate) fn lex_within(member: &[u8], min: &LexBound, max: &LexBound) -> bool {
    let above_min = match min {
        LexBound::NegInf => true,
        LexBound::PosInf => false, // nothing sorts above +inf
        LexBound::Incl(b) => member >= b.as_slice(),
        LexBound::Excl(b) => member > b.as_slice(),
    };
    let below_max = match max {
        LexBound::NegInf => false, // nothing sorts below -inf
        LexBound::PosInf => true,
        LexBound::Incl(b) => member <= b.as_slice(),
        LexBound::Excl(b) => member < b.as_slice(),
    };
    above_min && below_max
}
