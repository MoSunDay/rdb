//! Consumer-group management: XGROUP CREATE / DESTROY / SETID and the
//! group-scan helper behind `XINFO GROUPS` / `XINFO STREAM`.
//!
//! CREATE is the Lite "subscribe": it fixes the group's start position
//! (`$` = only new messages). The group record (kind 0x0E) carries the
//! committed watermark that survives crashes.

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

/// `XGROUP <CREATE|DESTROY|SETID> <stream> <group> [<id|$> [MKSTREAM]
/// [ORDERED [INFLIGHT <n>]]]`
/// plus `<CREATECONSUMER|DELCONSUMER> <stream> <group> <consumer>`
/// (consumer-registry rows in the kind-0x0F window, see `pel.rs`).
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
        b"createconsumer" => createconsumer(ctx).await,
        b"delconsumer" => delconsumer(ctx).await,
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
    // XGROUP CREATE <stream> <group> <id|$> [MKSTREAM] [ORDERED [INFLIGHT <n>]]
    // ORDERED (rdb extension): queue-exclusive ownership + ordered
    // delivery + in-flight cap -- Kafka partition-order semantics over
    // the PEL protocol (see `ordered`).
    if ctx.args.len() < 4 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup create' command",
        );
    }
    let (mut mkstream, mut ordered, mut inflight) = (false, false, 0u64);
    let mut i = 4;
    while i < ctx.args.len() {
        let a = &ctx.args[i];
        if a.eq_ignore_ascii_case(b"MKSTREAM") && !mkstream {
            mkstream = true;
        } else if a.eq_ignore_ascii_case(b"ORDERED") && !ordered {
            ordered = true;
        } else if a.eq_ignore_ascii_case(b"INFLIGHT") && inflight == 0 && i + 1 < ctx.args.len() {
            match std::str::from_utf8(&ctx.args[i + 1])
                .ok()
                .and_then(|t| t.parse::<u64>().ok())
            {
                Some(n) if n >= 1 => inflight = n,
                _ => {
                    return resp::append_error(
                        ctx.out,
                        "ERR value is not an integer or out of range",
                    )
                }
            }
            i += 1;
        } else {
            return resp::append_error(ctx.out, "ERR syntax error");
        }
        i += 1;
    }
    if inflight > 0 && !ordered {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    let group = ctx.args[2].clone();
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    let read = model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    );
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
    let inflight_max = model::normalize_inflight(ordered, inflight);
    batch.put(
        &gkey,
        model::encode_group(&GroupPayload {
            created_ms: now,
            delivered_ms: start.ms,
            delivered_seq: start.seq,
            committed_ms: start.ms,
            committed_seq: start.seq,
            ordered,
            inflight_max,
        }),
    );
    if let Err(e) = ctx.commit(batch).await {
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
    // Raw-byte cache key: group names are not charset-validated here.
    offset::insert_new(
        &ctx.shared.lite.offsets,
        &stream,
        &group,
        GroupState {
            created_ms: now,
            delivered: start,
            committed: start,
            pending: 0,
            ordered,
            inflight_max,
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
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    let gkey = model::group_key(&prefix, &stream, &group);
    let existed = ops::get_physical(&ctx.shared.store, &gkey)
        .ok()
        .flatten()
        .is_some();
    if existed {
        let mut batch = WriteBatch::default();
        batch.delete(&gkey);
        // The group's whole pending window (PEL rows + consumer
        // registry) goes with it: one range delete, no enumeration.
        super::pel::delete_group_pend(&mut batch, &prefix, &stream, &group);
        if let Err(e) = ctx.commit(batch).await {
            return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
        }
        // A consumer blocked in XREADGROUP on this stream must not park
        // out its BLOCK timeout: wake it so its re-check observes the
        // missing group and replies NOGROUP (CREATE above does the same).
        wait::notify(&ctx.shared.wait_hub, &model::meta_key(&prefix, &stream));
    }
    offset::remove_group(&ctx.shared.lite.offsets, &stream, &group);
    super::ordered::drop_group(&ctx.shared.lite.owners, &stream, &group);
    ctx.shared.lite.forget_group(&stream, &group);
    resp::append_int(ctx.out, i64::from(existed));
}

/// `XGROUP CREATECONSUMER <stream> <group> <consumer>`: register the
/// consumer name (idempotent from the delivery path too); replies 1 when
/// newly created, 0 when it already existed.
async fn createconsumer(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup createconsumer' command",
        );
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    let (group, consumer) = (ctx.args[2].clone(), ctx.args[3].clone());
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    let ckey = super::pel::consumer_key(&prefix, &stream, &group, &consumer);
    let existed = ops::get_physical(&ctx.shared.store, &ckey)
        .ok()
        .flatten()
        .is_some();
    if !existed {
        let mut batch = WriteBatch::default();
        batch.put(
            &ckey,
            super::pel::encode_consumer(&super::pel::ConsumerState {
                created_ms: crate::ds::expire::now_ms(),
            }),
        );
        if let Err(e) = ctx.commit(batch).await {
            return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
        }
    }
    ctx.shared.lite.ensure_consumer(&stream, &group, &consumer);
    resp::append_int(ctx.out, i64::from(!existed));
}

/// `XGROUP DELCONSUMER <stream> <group> <consumer>`: drop the registry
/// row and purge the consumer's pending entries; replies the number of
/// purged pending messages (Redis semantics).
async fn delconsumer(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 4 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xgroup delconsumer' command",
        );
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    let (group, consumer) = (ctx.args[2].clone(), ctx.args[3].clone());
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    // Purge every PEL row owned by this consumer (ownership lives in the
    // row value, so it is a filtered scan, not a range delete).
    let rows = super::pel::scan_pend(
        &ctx.shared.store,
        &prefix,
        &stream,
        &group,
        model::MIN_ID,
        None,
    );
    let Ok(rows) = rows else {
        return resp::append_error(ctx.out, "ERR: xgroup failed: pel scan");
    };
    let mut purged = 0usize;
    let mut batch = WriteBatch::default();
    for row in &rows {
        if row.state.consumer == consumer {
            batch.delete(super::pel::pend_key(&prefix, &stream, &group, row.id));
            purged += 1;
        }
    }
    batch.delete(super::pel::consumer_key(
        &prefix, &stream, &group, &consumer,
    ));
    if let Err(e) = ctx.commit(batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
    }
    offset::bump_pending(&ctx.shared.lite.offsets, &stream, &group, -(purged as i64));
    // An ordered queue owned by the departing consumer becomes free NOW
    // (no lease wait): the next `>` reader takes it over.
    super::ordered::release_consumer(&ctx.shared.lite.owners, &stream, &group, &consumer);
    ctx.shared.lite.forget_consumer(&stream, &group, &consumer);
    resp::append_int(ctx.out, purged as i64);
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
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
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
    let last = model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    )
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
    if let Err(e) = ctx.commit(batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
    }
    // The delivery watermark may have moved (a rewind makes previously
    // consumed entries deliverable again): wake blocked `>` readers so
    // they re-check instead of sleeping out their BLOCK timeout.
    wait::notify(&ctx.shared.wait_hub, &model::meta_key(&prefix, &stream));
    offset::set_position(&ctx.shared.lite.offsets, &stream, &group, id);
    resp::append_string(ctx.out, "OK");
}
