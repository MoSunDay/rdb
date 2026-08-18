//! Vector-set commands, part 1 (VADD/VREM/VCARD/VDIM) plus the plumbing
//! every vector-set handler shares (mirrors how `zset_util` hosts the
//! state helper the read/pop modules import): keys resolve via
//! `keys_core::resolve` (lazy expiry + wrong-type detection), elements
//! live in `ds::vectorset_ds` records and every mutation lands in ONE
//! batched fsync under the per-key latch. Attributes (VSETATTR/VGETATTR)
//! live in `vectorset_attr`, similarity search (VSIM) in `vectorset_sim`.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_f64, WRONGTYPE};
use crate::command::zset_util::eq_ignore_case;
use crate::command::{keys_core, Ctx};
use crate::ds::codec::KIND_VECTORSET_META;
use crate::ds::{expire, latch, vectorset_ds};
use crate::resp::codec::{append_error, append_int};
use crate::store::{ops, Store};

/// Missing key on the commands that require one (VDIM/VSIM).
pub(crate) const ERR_NO_KEY: &str = "ERR vector set does not exist";
/// Dimension outside 1..=4096.
pub(crate) const ERR_DIM: &str = "ERR invalid dim";
/// VADD against an existing set of another dimension.
pub(crate) const ERR_DIM_MISMATCH: &str = "ERR dimension mismatch";
/// FP16 blob whose byte length is not dim*2.
pub(crate) const ERR_FP16: &str = "ERR invalid FP16 vector";
/// VALUES tail that is not exactly dim finite-parseable f64s.
pub(crate) const ERR_VALUE: &str = "ERR invalid vector value";
/// VSIM COUNT argument that is not a u64.
pub(crate) const ERR_COUNT: &str = "ERR invalid COUNT";

/// What one key is from the vector-set commands' point of view.
#[derive(Debug, PartialEq)]
pub(crate) enum VectorSetState {
    Missing,
    WrongType,
    VectorSet {
        expire_ms: u64,
        dim: u64,
        count: u64,
    },
}

/// Resolve via keys_core (raw strings and foreign kinds -> WrongType);
/// an expired vector set purges and reads as Missing.
pub(crate) fn vectorset_state(
    store: &Store,
    prefix: &[u8],
    key: &[u8],
    now: u64,
) -> VectorSetState {
    match keys_core::resolve(store, prefix, key, now) {
        keys_core::KeyState::Missing => VectorSetState::Missing,
        keys_core::KeyState::RawString { .. } => VectorSetState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_VECTORSET_META => {
            VectorSetState::WrongType
        }
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => {
            let (dim, count) = vectorset_ds::decode_meta(&payload);
            VectorSetState::VectorSet {
                expire_ms,
                dim,
                count,
            }
        }
    }
}

/// Dimension argument: a u64 in 1..=4096 (Redis' own cap).
fn parse_dim(arg: &[u8]) -> Option<u64> {
    let dim: u64 = std::str::from_utf8(arg).ok()?.parse().ok()?;
    (1..=4096).contains(&dim).then_some(dim)
}

/// Decode one vector argument tail shared by VADD/VSIM: `mode` FP16
/// consumes exactly one blob of dim*2 LE u16s, VALUES consumes exactly
/// dim f64 literals. The Err text is the mode-specific reply.
pub(crate) fn parse_vector(
    mode: &[u8],
    args: &[Vec<u8>],
    dim: u64,
) -> Result<Vec<f64>, &'static str> {
    if eq_ignore_case(mode, b"FP16") {
        if args.len() != 1 || args[0].len() != dim as usize * 2 {
            return Err(ERR_FP16);
        }
        return Ok(args[0]
            .chunks_exact(2)
            .map(|c| vectorset_ds::fp16_to_f64(u16::from_le_bytes([c[0], c[1]])))
            .collect());
    }
    if eq_ignore_case(mode, b"VALUES") {
        if args.len() != dim as usize {
            return Err(ERR_VALUE);
        }
        return args
            .iter()
            .map(|a| parse_f64(a))
            .collect::<Option<Vec<f64>>>()
            .ok_or(ERR_VALUE);
    }
    Err("ERR syntax error")
}

/// Land one batched fsync; on failure the error reply is written here.
async fn commit(ctx: &mut Ctx<'_>, batch: WriteBatch, cmd: &str) -> bool {
    ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .map(|_| true)
        .unwrap_or_else(|_| {
            append_error(ctx.out, &format!("ERR: {cmd} failed"));
            false
        })
}

/// VADD key (FP16|VALUES) dim element <vector...> -> :1 when the
/// element was added, :0 when it already existed (vector overwritten,
/// attribute PRESERVED, count unchanged). Re-adds keep the key's TTL.
pub async fn vadd(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 5 {
        arity(ctx.out, "vadd");
        return;
    }
    let key = ctx.args[0].clone();
    let Some(dim) = parse_dim(&ctx.args[2]) else {
        append_error(ctx.out, ERR_DIM);
        return;
    };
    let vector = match parse_vector(&ctx.args[1], &ctx.args[4..], dim) {
        Ok(v) => v,
        Err(e) => {
            append_error(ctx.out, e);
            return;
        }
    };
    let element = ctx.args[3].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let (expire_ms, count) =
        match vectorset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
            VectorSetState::WrongType => {
                append_error(ctx.out, WRONGTYPE);
                return;
            }
            VectorSetState::Missing => (0, 0),
            VectorSetState::VectorSet {
                expire_ms,
                dim: live_dim,
                count,
            } => {
                if live_dim != dim {
                    append_error(ctx.out, ERR_DIM_MISMATCH);
                    return;
                }
                (expire_ms, count)
            }
        };
    let existing =
        match vectorset_ds::read_elem(&ctx.shared.store, &ctx.prefix_key, &key, &element, dim) {
            Ok(e) => e,
            Err(_) => {
                append_error(ctx.out, "ERR: vadd failed");
                return;
            }
        };
    // Re-adding overwrites the vector but PRESERVES the stored
    // attribute; only genuinely new elements bump the count.
    let (count, added, attr) = match existing {
        Some((_, attr)) => (count, false, attr),
        None => (count + 1, true, None),
    };
    let mut batch = WriteBatch::default();
    vectorset_ds::put_elem(
        &mut batch,
        &ctx.prefix_key,
        &key,
        &element,
        dim,
        &vector,
        attr.as_deref(),
    );
    // Invariant: count > 0 always -- VADD only adds, an empty vector
    // set never exists (VREM family-deletes at zero, like hashes/zsets).
    vectorset_ds::write_meta(
        &mut batch,
        &ctx.prefix_key,
        &key,
        expire_ms,
        expire_ms,
        dim,
        count,
    );
    if commit(ctx, batch, "vadd").await {
        append_int(ctx.out, i64::from(added));
    }
}

/// VREM key element -> :1 when removed (count-1; hitting zero wipes the
/// whole family, TTL index included), :0 for a missing key/element.
pub async fn vrem(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "vrem");
        return;
    }
    let (key, element) = (ctx.args[0].clone(), ctx.args[1].clone());
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    match vectorset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        VectorSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        VectorSetState::Missing => append_int(ctx.out, 0),
        VectorSetState::VectorSet {
            expire_ms,
            dim,
            count,
        } => {
            let exists =
                vectorset_ds::read_elem(&ctx.shared.store, &ctx.prefix_key, &key, &element, dim)
                    .map(|e| e.is_some())
                    .unwrap_or(false);
            if !exists {
                append_int(ctx.out, 0);
                return;
            }
            let mut batch = WriteBatch::default();
            batch.delete(vectorset_ds::elem_key(&ctx.prefix_key, &key, &element));
            if count == 1 {
                // Last element: empty vector sets do not exist.
                vectorset_ds::delete_family(&mut batch, &ctx.prefix_key, &key, expire_ms);
            } else {
                vectorset_ds::write_meta(
                    &mut batch,
                    &ctx.prefix_key,
                    &key,
                    expire_ms,
                    expire_ms,
                    dim,
                    count - 1,
                );
            }
            if commit(ctx, batch, "vrem").await {
                append_int(ctx.out, 1);
            }
        }
    }
}

/// VCARD key -> :count, 0 when the key does not exist.
pub async fn vcard(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "vcard");
        return;
    }
    match vectorset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        VectorSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        VectorSetState::Missing => append_int(ctx.out, 0),
        VectorSetState::VectorSet { count, .. } => append_int(ctx.out, count as i64),
    }
}

/// VDIM key -> :dim; a missing key is an error (like VRANDEMBER's kin).
pub async fn vdim(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "vdim");
        return;
    }
    match vectorset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        VectorSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        VectorSetState::Missing => append_error(ctx.out, ERR_NO_KEY),
        VectorSetState::VectorSet { dim, .. } => append_int(ctx.out, dim as i64),
    }
}
