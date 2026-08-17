//! Set commands (SADD..SSCAN): members are stored as existence-encoded
//! records via `ds::set_ds`; every mutation runs under the per-key latch
//! in ONE batched fsync. Multi-key set algebra (SUNION/SINTER/SDIFF and
//! their STORE twins) lives in `setops_cmd`; SMOVE stays here because it
//! is a member move, not set algebra.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::hash_cmd::{arity, parse_i64, WRONGTYPE};
use crate::command::{keys_core, Ctx};
use crate::ds::codec::{self, KIND_SET_META};
use crate::ds::{expire, latch, set_ds};
use crate::resp::codec::{append_array, append_bulk, append_error, append_int, append_null};
use crate::store::ops;
use crate::utils::rand_u64;

/// What one key is from the set commands' point of view.
#[derive(Debug, PartialEq)]
pub(crate) enum SetState {
    Missing,
    Set { expire_ms: u64, count: u64 },
    WrongType,
}

/// Resolve via keys_core: raw strings/foreign kinds -> WrongType; an
/// expired set purges and reads as Missing.
pub(crate) fn set_state(ctx: &Ctx<'_>, key: &[u8]) -> SetState {
    match keys_core::resolve(&ctx.shared.store, &ctx.prefix_key, key, expire::now_ms()) {
        keys_core::KeyState::Missing => SetState::Missing,
        keys_core::KeyState::RawString { .. } => SetState::WrongType,
        keys_core::KeyState::Enveloped { kind, .. } if kind != KIND_SET_META => SetState::WrongType,
        keys_core::KeyState::Enveloped {
            expire_ms, payload, ..
        } => SetState::Set {
            expire_ms,
            count: codec::decode_count(&payload),
        },
    }
}

/// Meta for a write path: `(expire_ms, count)`, replying WRONGTYPE and
/// answering `None` when the key holds another type.
fn write_meta_of(ctx: &mut Ctx<'_>, key: &[u8]) -> Option<(u64, u64)> {
    match set_state(ctx, key) {
        SetState::Set { expire_ms, count } => Some((expire_ms, count)),
        SetState::Missing => Some((0, 0)),
        SetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            None
        }
    }
}

fn has_member(ctx: &Ctx<'_>, key: &[u8], member: &[u8]) -> bool {
    set_ds::has_member(&ctx.shared.store, &ctx.prefix_key, key, member).unwrap_or(false)
}

/// Commit one set mutation: member puts/deletes plus the meta record (or a
/// family wipe at count 0), single fsync; the error reply is written here.
async fn commit(
    ctx: &mut Ctx<'_>,
    key: &[u8],
    expire_ms: u64,
    count: u64,
    adds: &[Vec<u8>],
    removes: &[Vec<u8>],
    cmd: &str,
) -> Result<(), ()> {
    let mut batch = WriteBatch::default();
    for m in adds {
        batch.put(set_ds::member_key(&ctx.prefix_key, key, m), b"");
    }
    for m in removes {
        batch.delete(set_ds::member_key(&ctx.prefix_key, key, m));
    }
    if count == 0 {
        // Empty sets do not exist (Redis semantics).
        set_ds::delete_family(&mut batch, &ctx.prefix_key, key, expire_ms);
    } else {
        set_ds::write_meta(&mut batch, &ctx.prefix_key, key, expire_ms, count);
    }
    ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .map_err(|_| append_error(ctx.out, &format!("ERR: {cmd} failed")))
}

/// SADD key member [member ...] -> count of NEW members.
pub async fn sadd(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "sadd");
        return;
    }
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    let mut fresh: Vec<Vec<u8>> = Vec::new();
    for m in &ctx.args[1..] {
        if !fresh.contains(m) && !has_member(ctx, &key, m) {
            fresh.push(m.clone());
        }
    }
    let count = base + fresh.len() as u64;
    if commit(ctx, &key, expire_ms, count, &fresh, &[], "sadd")
        .await
        .is_ok()
    {
        append_int(ctx.out, fresh.len() as i64);
    }
}

/// SREM key member [member ...] -> count removed; the LAST member deletes
/// the whole set.
pub async fn srem(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "srem");
        return;
    }
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    if base == 0 {
        append_int(ctx.out, 0);
        return;
    }
    let mut gone: Vec<Vec<u8>> = Vec::new();
    for m in &ctx.args[1..] {
        if !gone.contains(m) && has_member(ctx, &key, m) {
            gone.push(m.clone());
        }
    }
    let remaining = base.saturating_sub(gone.len() as u64);
    if commit(ctx, &key, expire_ms, remaining, &[], &gone, "srem")
        .await
        .is_ok()
    {
        append_int(ctx.out, gone.len() as i64);
    }
}

/// SMEMBERS key -> every member (missing key -> empty array).
pub async fn smembers(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "smembers");
        return;
    }
    if let SetState::WrongType = set_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    let page = set_ds::collect_members(
        &ctx.shared.store,
        &ctx.prefix_key,
        &ctx.args[0],
        None,
        None,
        0,
    );
    append_array(ctx.out, page.members.len());
    for m in &page.members {
        append_bulk(ctx.out, m);
    }
}

/// SISMEMBER key member -> 0/1.
pub async fn sismember(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 2 {
        arity(ctx.out, "sismember");
        return;
    }
    let answer = match set_state(ctx, &ctx.args[0]) {
        SetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        SetState::Missing => false,
        SetState::Set { .. } => has_member(ctx, &ctx.args[0], &ctx.args[1]),
    };
    append_int(ctx.out, i64::from(answer));
}

/// SMISMEMBER key member [member ...] -> array of 0/1 flags.
pub async fn smismember(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        arity(ctx.out, "smismember");
        return;
    }
    if let SetState::WrongType = set_state(ctx, &ctx.args[0]) {
        append_error(ctx.out, WRONGTYPE);
        return;
    }
    append_array(ctx.out, ctx.args.len() - 1);
    for m in &ctx.args[1..] {
        append_int(ctx.out, i64::from(has_member(ctx, &ctx.args[0], m)));
    }
}

/// SCARD key -> member count (0 for missing keys).
pub async fn scard(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        arity(ctx.out, "scard");
        return;
    }
    match set_state(ctx, &ctx.args[0]) {
        SetState::Set { count, .. } => append_int(ctx.out, count as i64),
        SetState::Missing => append_int(ctx.out, 0),
        SetState::WrongType => append_error(ctx.out, WRONGTYPE),
    }
}

/// `SPOP key [count]`: remove and return random members. count must be
/// positive; distinct members are popped (up to the cardinality). Without
/// count: one bulk member or null.
pub async fn spop(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() || ctx.args.len() > 2 {
        arity(ctx.out, "spop");
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
    if count == Some(0) {
        append_error(ctx.out, "ERR value is out of range, must be positive");
        return;
    }
    let key = ctx.args[0].clone();
    let _guard = latch::lock(
        &ctx.shared.latch,
        &keys_core::latch_key(&ctx.prefix_key, &key),
    );
    let Some((expire_ms, base)) = write_meta_of(ctx, &key) else {
        return;
    };
    if base == 0 {
        match count {
            None => append_null(ctx.out),
            Some(_) => append_array(ctx.out, 0),
        }
        return;
    }
    let page = set_ds::collect_members(&ctx.shared.store, &ctx.prefix_key, &key, None, None, 0);
    let want = match count {
        None => 1,
        Some(n) => (n as u64).min(page.members.len() as u64),
    };
    // Random distinct picks by shuffling indices (Fisher-Yates over a
    // prefix of length `want`): the first `want` slots end up random.
    let mut idx: Vec<usize> = (0..page.members.len()).collect();
    for i in 0..want as usize {
        let j = i + (rand_u64() % (idx.len() - i) as u64) as usize;
        idx.swap(i, j);
    }
    let picked: Vec<Vec<u8>> = idx[..want as usize]
        .iter()
        .map(|&i| page.members[i].clone())
        .collect();
    let remaining = base.saturating_sub(want);
    if commit(ctx, &key, expire_ms, remaining, &[], &picked, "spop")
        .await
        .is_ok()
    {
        match count {
            None => append_bulk(ctx.out, &picked[0]),
            Some(_) => {
                append_array(ctx.out, picked.len());
                for m in &picked {
                    append_bulk(ctx.out, m);
                }
            }
        }
    }
}

/// `SMOVE source dest member`: 1 when moved. Requires both keys to hash to
/// the same slot (cluster rule); locks both latch keys in byte order.
/// Moving into the same key answers by membership only.
pub async fn smove(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        arity(ctx.out, "smove");
        return;
    }
    let (src, dst, member) = (
        ctx.args[0].clone(),
        ctx.args[1].clone(),
        ctx.args[2].clone(),
    );
    if !crate::ds::setops::same_slot(&[src.clone(), dst.clone()]) {
        append_error(ctx.out, crate::ds::setops::CROSSSLOT_ERROR);
        return;
    }
    if src == dst {
        if let SetState::WrongType = set_state(ctx, &src) {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
        append_int(ctx.out, i64::from(has_member(ctx, &src, &member)));
        return;
    }
    // Latch both keys in byte order (ABBA rule; see ds::latch docs).
    let mut guards = Vec::new();
    for k in [&src, &dst] {
        guards.push(latch::lock(
            &ctx.shared.latch,
            &keys_core::latch_key(&ctx.prefix_key, k),
        ));
    }
    let (src_expire, src_base) = match set_state(ctx, &src) {
        SetState::Set { expire_ms, count } => (expire_ms, count),
        SetState::Missing => {
            append_int(ctx.out, 0);
            return;
        }
        SetState::WrongType => {
            append_error(ctx.out, WRONGTYPE);
            return;
        }
    };
    if !has_member(ctx, &src, &member) {
        append_int(ctx.out, 0);
        return;
    }
    let (dst_expire, dst_base) = match write_meta_of(ctx, &dst) {
        Some(pair) => pair,
        None => return,
    };
    let mut batch = WriteBatch::default();
    // dst_base + 1 also creates the destination meta when it was missing.
    set_ds::write_meta(&mut batch, &ctx.prefix_key, &dst, dst_expire, dst_base + 1);
    batch.put(set_ds::member_key(&ctx.prefix_key, &dst, &member), b"");
    if src_base <= 1 {
        set_ds::delete_family(&mut batch, &ctx.prefix_key, &src, src_expire);
    } else {
        batch.delete(set_ds::member_key(&ctx.prefix_key, &src, &member));
        set_ds::write_meta(&mut batch, &ctx.prefix_key, &src, src_expire, src_base - 1);
    }
    if ops::batch_write_async(Arc::clone(&ctx.shared.store), batch)
        .await
        .is_ok()
    {
        append_int(ctx.out, 1);
    } else {
        append_error(ctx.out, "ERR: smove failed");
    }
}
