//! Vector-set attribute commands (VSETATTR/VGETATTR). Attributes ride
//! INSIDE the elem record (LEB128 attr_len ++ bytes after the vector),
//! so both handlers rewrite/read the whole elem value through
//! `ds::vectorset_ds`; the meta record is attribute-independent and is
//! never touched here (count and TTL unchanged). Exactly like the rest
//! of the family: keys resolve via `keys_core::resolve`, writes land in
//! ONE batched fsync under the per-key latch.

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, WRONGTYPE};
use crate::command::vectorset_cmd::{vectorset_state, VectorSetState};
use crate::command::{keys_core, Ctx};
use crate::ds::{expire, latch, vectorset_ds};
use crate::resp::codec::{append_bulk, append_error, append_int, append_null};

/// VSETATTR key element attr -> :1 when the element existed (attr
/// rewritten, vector untouched), :0 otherwise. An empty attr clears the
/// attribute (attr_len 0). Single fsync: one elem record rewrite.
pub async fn vsetattr(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "vsetattr");
        return;
    }
    let (key, element, attr) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    )
    .await;
    let dim = match vectorset_state(&ctx.shared.store, &ctx.prefix_key, &key, expire::now_ms()) {
        VectorSetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        VectorSetState::Missing => {
            append_int(ctx.out, 0);
            return;
        }
        VectorSetState::VectorSet { dim, .. } => dim,
    };
    // Only a live element can carry an attribute; its vector is decoded
    // and re-encoded unchanged (attr_len 0 for the empty string). A read
    // error must not masquerade as "element missing" (vgetattr errs too).
    let vector =
        match vectorset_ds::read_elem(&ctx.shared.store, &ctx.prefix_key, &key, &element, dim) {
            Ok(Some((vector, _))) => vector,
            Ok(None) => {
                append_int(ctx.out, 0);
                return;
            }
            Err(_) => {
                append_error(ctx.out, "ERR: vsetattr failed");
                return;
            }
        };
    let mut batch = WriteBatch::default();
    vectorset_ds::put_elem(
        &mut batch,
        &ctx.prefix_key,
        &key,
        &element,
        dim,
        &vector,
        Some(&attr),
    );
    match ctx.commit(batch).await {
        Ok(()) => append_int(ctx.out, 1),
        Err(_) => append_error(ctx.out, "ERR: vsetattr failed"),
    }
}

/// VGETATTR key element -> bulk attr bytes; null bulk when the key or
/// element is missing, or the element simply has no attribute set.
pub async fn vgetattr(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "vgetattr");
        return;
    }
    match vectorset_state(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        expire::now_ms(),
    ) {
        VectorSetState::WrongType => append_error(ctx.out, WRONGTYPE),
        VectorSetState::Missing => append_null(ctx.out),
        VectorSetState::VectorSet { dim, .. } => {
            match vectorset_ds::read_elem(
                &ctx.shared.store,
                &ctx.prefix_key,
                &ctx.args[0],
                &ctx.args[1],
                dim,
            ) {
                Ok(Some((_, Some(attr)))) => append_bulk(ctx.out, &attr),
                Ok(_) => append_null(ctx.out),
                Err(_) => append_error(ctx.out, "ERR: vgetattr failed"),
            }
        }
    }
}
