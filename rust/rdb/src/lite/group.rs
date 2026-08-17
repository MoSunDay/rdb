//! Consumer-group management: XGROUP CREATE / DESTROY / SETID and the
//! group-scan helper behind `XINFO GROUPS` / `XINFO STREAM`.
//!
//! CREATE is the Lite "subscribe": it fixes the group's start position
//! (`$` = only new messages). The group record (kind 0x0E) carries the
//! committed watermark that survives crashes.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::Ctx;
use crate::ds::{latch, wait};
use crate::resp::codec as resp;
use crate::store::ops;

use super::model::{self, EntryId, GroupPayload, MetaRead};
use super::offset::{self, GroupState};

/// All groups of one stream, ordered by name.
pub fn groups_of(
    store: &crate::store::Store,
    prefix: &[u8],
    stream: &[u8],
) -> Result<Vec<(Vec<u8>, GroupPayload)>, String> {
    let base = model::group_key(prefix, stream, b"");
    let mut out = Vec::new();
    ops::for_each_from(store, &base, false, &mut |k, v| {
        if !k.starts_with(&base) {
            return false;
        }
        let name = k[base.len()..].to_vec();
        if let Some(p) = model::decode_group(v) {
            out.push((name, p));
        }
        true
    })?;
    Ok(out)
}

fn group_start(stream_last: Option<EntryId>, arg: &[u8]) -> Result<EntryId, ()> {
    if arg == b"$" {
        Ok(stream_last.unwrap_or(model::MIN_ID))
    } else {
        model::parse_id(arg).ok_or(())
    }
}

/// `XGROUP <CREATE|DESTROY|SETID> <stream> <group> [<id|$> [MKSTREAM]]`.
pub async fn xgroup(ctx: &mut Ctx<'_>) {
    if ctx.args.is_empty() {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup' command",
        );
    }
    let sub = ctx.args[0].to_ascii_lowercase();
    match sub.as_slice() {
        b"create" => create(ctx).await,
        b"destroy" => destroy(ctx).await,
        b"setid" => setid(ctx).await,
        _ => resp::append_error(
            ctx.out,
            &format!(
                "ERR Unknown subcommand for '{}'",
                String::from_utf8_lossy(&ctx.args[0])
            ),
        ),
    }
}

async fn create(ctx: &mut Ctx<'_>) {
    // XGROUP CREATE <stream> <group> <id|$> [MKSTREAM]
    let mkstream = ctx.args.len() == 5 && ctx.args[4].eq_ignore_ascii_case(b"MKSTREAM");
    if !matches!(ctx.args.len(), 4 | 5) || (ctx.args.len() == 5 && !mkstream) {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup create' command",
        );
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    let group = ctx.args[2].clone();
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream));
    let read = model::read_meta(&ctx.shared.store, &prefix, &stream);
    let meta = match &read {
        Ok(MetaRead::Live(m)) => Some(m.clone()),
        Ok(MetaRead::Purged) | Ok(MetaRead::Missing) => None,
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}")),
    };
    let fresh_meta = meta.is_none();
    let gkey = model::group_key(&prefix, &stream, &group);
    if ops::get_physical(&ctx.shared.store, &gkey)
        .ok()
        .flatten()
        .is_some()
    {
        return resp::append_error(ctx.out, "BUSYGROUP Consumer Group name already exists");
    }
    if fresh_meta && !mkstream {
        return resp::append_error(
            ctx.out,
            "ERR The XGROUP subcommand requires the key to exist. Note that for CREATE you may want to use the MKSTREAM option.",
        );
    }
    let Ok(start) = group_start(meta.as_ref().map(|m| m.last_id()), &ctx.args[3]) else {
        return resp::append_error(
            ctx.out,
            "ERR Invalid stream ID specified as stream command argument",
        );
    };
    let now = crate::ds::expire::now_ms();
    let mut batch = WriteBatch::default();
    if fresh_meta {
        let fresh = model::MetaPayload {
            created_ms: now,
            ..Default::default()
        };
        batch.put(
            model::meta_key(&prefix, &stream),
            model::encode_meta(&fresh),
        );
    }
    batch.put(
        &gkey,
        model::encode_group(&GroupPayload {
            created_ms: now,
            delivered_ms: start.ms,
            delivered_seq: start.seq,
            committed_ms: start.ms,
            committed_seq: start.seq,
        }),
    );
    if let Err(e) = ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
    }
    if fresh_meta {
        let stats = &ctx.shared.lite.stats;
        if matches!(read, Ok(MetaRead::Purged)) {
            super::entries::count_reap(ctx);
        }
        stats
            .streams_live
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let stream_s = String::from_utf8_lossy(&stream).into_owned();
    let group_s = String::from_utf8_lossy(&group).into_owned();
    offset::insert_new(
        &ctx.shared.lite.offsets,
        &stream_s,
        &group_s,
        GroupState {
            created_ms: now,
            delivered: start,
            committed: start,
        },
    );
    wait::notify(&ctx.shared.wait_hub, &model::meta_key(&prefix, &stream));
    resp::append_string(ctx.out, "OK");
}

async fn destroy(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 3 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup destroy' command",
        );
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    let group = ctx.args[2].clone();
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream));
    let gkey = model::group_key(&prefix, &stream, &group);
    let existed = ops::get_physical(&ctx.shared.store, &gkey)
        .ok()
        .flatten()
        .is_some();
    if existed {
        let mut batch = WriteBatch::default();
        batch.delete(&gkey);
        if let Err(e) = ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
            return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
        }
    }
    offset::remove_group(
        &ctx.shared.lite.offsets,
        &String::from_utf8_lossy(&stream),
        &String::from_utf8_lossy(&group),
    );
    resp::append_int(ctx.out, i64::from(existed));
}

async fn setid(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup setid' command",
        );
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    let group = ctx.args[2].clone();
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream));
    let gkey = model::group_key(&prefix, &stream, &group);
    let Some(raw) = ops::get_physical(&ctx.shared.store, &gkey).ok().flatten() else {
        return resp::append_error(
            ctx.out,
            &format!(
                "NOGROUP No such key '{}' or consumer group '{}'",
                String::from_utf8_lossy(&stream),
                String::from_utf8_lossy(&group)
            ),
        );
    };
    let Some(mut payload) = model::decode_group(&raw) else {
        return resp::append_error(ctx.out, "ERR: corrupt group record");
    };
    let last = model::read_meta(&ctx.shared.store, &prefix, &stream)
        .ok()
        .and_then(|r| r.live())
        .map(|m| m.last_id());
    let Ok(id) = group_start(last, &ctx.args[3]) else {
        return resp::append_error(
            ctx.out,
            "ERR Invalid stream ID specified as stream command argument",
        );
    };
    payload.delivered_ms = id.ms;
    payload.delivered_seq = id.seq;
    payload.committed_ms = id.ms;
    payload.committed_seq = id.seq;
    let mut batch = WriteBatch::default();
    batch.put(&gkey, model::encode_group(&payload));
    if let Err(e) = ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
    }
    offset::set_position(
        &ctx.shared.lite.offsets,
        &String::from_utf8_lossy(&stream),
        &String::from_utf8_lossy(&group),
        id,
    );
    resp::append_string(ctx.out, "OK");
}
