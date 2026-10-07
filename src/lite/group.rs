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
    let (mut maxdelivery, mut dlq, mut dlq_seen) = (0u64, Vec::new(), false);
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
        } else if a.eq_ignore_ascii_case(b"MAXDELIVERY")
            && maxdelivery == 0
            && i + 1 < ctx.args.len()
        {
            match std::str::from_utf8(&ctx.args[i + 1])
                .ok()
                .and_then(|t| t.parse::<u64>().ok())
            {
                Some(n) if n >= 1 => maxdelivery = n,
                _ => {
                    return resp::append_error(
                        ctx.out,
                        "ERR value is not an integer or out of range",
                    )
                }
            }
            i += 1;
        } else if a.eq_ignore_ascii_case(b"DLQ") && !dlq_seen && i + 1 < ctx.args.len() {
            dlq_seen = true;
            dlq = ctx.args[i + 1].clone();
            i += 1;
        } else {
            return resp::append_error(ctx.out, "ERR syntax error");
        }
        i += 1;
    }
    if inflight > 0 && !ordered {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    // DLQ rides a MAXDELIVERY cap (the INFLIGHT-must-follow-ORDERED
    // precedent): a target without a cap would never transfer. An
    // EXPLICIT option counts as configured even when the name is empty
    // (the empty name gets its own dedicated refusal below).
    if dlq_seen && maxdelivery == 0 {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    let Some((stream, prefix)) = super::entries::stream_of(ctx, 1) else {
        return;
    };
    // Resolve the final target once, here: an explicit name verbatim
    // (any topic/slot), else the literal same-slot default `<stream>/
    // dlq`. Both must survive topic-name validation so the DLQ stays a
    // normal, independently consumable stream.
    if maxdelivery > 0 {
        if dlq.is_empty() {
            if dlq_seen {
                // `DLQ ""` is an explicit EMPTY name, not an omitted
                // option: silently falling back to the default would
                // retarget the group's dead letters somewhere the
                // caller never named -- refuse instead of guessing.
                return resp::append_error(ctx.out, "ERR empty DLQ target name");
            }
            dlq = super::dlq::default_dlq_stream(&stream);
        }
        match super::parse_topic_name(&dlq) {
            Ok(super::TopicName::Stream(p, c)) => {
                let mut canon = p;
                canon.push(b'/');
                canon.extend_from_slice(&c);
                dlq = canon;
            }
            _ => return resp::append_error(ctx.out, "ERR invalid DLQ stream name"),
        }
        // The dead-letter transfer XADDs into the target, so a target
        // that IS the source would overwrite the business payload in
        // place and permanently inflate the stream's len.
        if dlq == stream {
            return resp::append_error(ctx.out, "ERR DLQ target must not be the source stream");
        }
        // Cluster-form safety: physical storage is `<slot>/`-prefixed
        // (the PARENT-derived CRC16 slot), so an explicit target in a
        // DIFFERENT slot would land on another node's window -- only
        // same-slot targets are accepted.
        if model::stream_prefix(&dlq) != Some(prefix.clone()) {
            return resp::append_error(
                ctx.out,
                "ERR DLQ target must hash to the same slot as the source stream",
            );
        }
    }
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
            maxdelivery,
            dlq: dlq.clone(),
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
            maxdelivery,
            dlq,
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
    // The group's kafka committed-offset ledger rows (kind 0x20) fold
    // with it: the XTRIM/XDEL ledger guard trips on ANY 0x20 row of
    // the stream, so rows surviving the teardown would lock the
    // stream's retention forever. A kafka-only group (ledger rows but
    // no lite group record) is destroyable the same way -- and counts
    // as having existed, so the reply tells the operator it did
    // something.
    let mut batch = WriteBatch::default();
    let ledger_rows =
        match fold_group_ledger(&mut batch, &ctx.shared.store, &prefix, &stream, &group) {
            Ok(n) => n,
            Err(e) => return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}")),
        };
    if existed || ledger_rows > 0 {
        if existed {
            batch.delete(&gkey);
            // The group's whole pending window (PEL rows + consumer
            // registry) goes with it: one range delete, no enumeration.
            super::pel::delete_group_pend(&mut batch, &prefix, &stream, &group);
        }
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
    resp::append_int(ctx.out, i64::from(existed || ledger_rows > 0));
}

/// Enumerate this `(stream, group)`'s kafka committed-offset ledger
/// rows (kind 0x20) and queue their deletes into `batch`; returns the
/// row count (`Err` = the scan failed: a partial fold must never
/// commit). Bounded to the stream's own 0x20 window (the
/// length-prefixed `data_key` cannot bleed into a neighbouring
/// stream's window); `ledger::parse_key` decodes the full (stream,
/// group) identity, so a group whose name prefixes another (`g1` vs
/// `g10`) never swallows the longer one's rows. Safe against
/// concurrent kafka commits without extra latching: commits take the
/// SAME stream meta latch the caller already holds.
fn fold_group_ledger(
    batch: &mut WriteBatch,
    store: &crate::store::Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> Result<usize, String> {
    let window = crate::ds::codec::data_key(prefix, crate::ds::codec::KIND_STREAM_OFFSET, stream);
    let mut count = 0usize;
    ops::for_each_from(store, &window, false, &mut |k, _| {
        if !k.starts_with(&window) {
            return false; // left the stream's kind-0x20 window
        }
        if crate::kafka::ledger::parse_key(k) == Some((stream.to_vec(), group.to_vec())) {
            batch.delete(k);
            count += 1;
        }
        true
    })?;
    Ok(count)
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
    let existing = ops::get_physical(&ctx.shared.store, &ckey)
        .ok()
        .flatten()
        .and_then(|raw| super::pel::decode_consumer(&raw));
    if existing.is_none() {
        let now = crate::ds::expire::now_ms();
        let mut batch = WriteBatch::default();
        batch.put(
            &ckey,
            super::pel::encode_consumer(&super::pel::ConsumerState {
                created_ms: now,
                seen_ms: now,
            }),
        );
        if let Err(e) = ctx.commit(batch).await {
            return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
        }
    }
    // Remembered either way -- with the SURVIVING on-disk creation time
    // when the row predates this process, so later seen refreshes never
    // drift `created_ms`.
    let was_new = existing.is_none();
    let created = existing
        .as_ref()
        .map_or_else(crate::ds::expire::now_ms, |s| s.created_ms);
    ctx.shared
        .lite
        .ensure_consumer(&stream, &group, &consumer, created);
    resp::append_int(ctx.out, i64::from(was_new));
}

/// Batch plan of one or more consumer removals of a group -- the shared
/// body of `XGROUP DELCONSUMER` and the idle consumer GC (see
/// [`super::consumer_gc`]): delete every PEL row the consumer owns
/// (ownership lives in the row value, so it is a filtered pass over the
/// caller's pre-scanned `rows`, not a range delete) plus the registry
/// row itself. Returns the purged pending-row count (the DELCONSUMER
/// reply; the GC path only ever passes PEL-empty names, so it sees 0).
pub(crate) fn plan_consumer_removals(
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
    consumers: &[&[u8]],
    rows: &[super::pel::PendRow],
) -> (WriteBatch, usize) {
    let mut purged = 0usize;
    let mut batch = WriteBatch::default();
    for row in rows {
        if consumers.iter().any(|c| row.state.consumer == *c) {
            batch.delete(super::pel::pend_key(prefix, stream, group, row.id));
            purged += 1;
        }
    }
    for c in consumers {
        batch.delete(super::pel::consumer_key(prefix, stream, group, c));
    }
    (batch, purged)
}

/// Post-commit effects of a consumer removal, shared by XGROUP
/// DELCONSUMER and the idle consumer GC: cached backlog unwind, ordered
/// ownership release and the runtime registry forget -- so the two
/// removal paths can never disagree on counters or registries.
pub(crate) fn consumer_removal_effects(
    shared: &crate::state::Shared,
    stream: &[u8],
    group: &[u8],
    consumer: &[u8],
    purged: usize,
) {
    offset::bump_pending(&shared.lite.offsets, stream, group, -(purged as i64));
    // An ordered queue owned by the departing consumer becomes free NOW
    // (no lease wait): the next `>` reader takes it over.
    super::ordered::release_consumer(&shared.lite.owners, stream, group, consumer);
    shared.lite.forget_consumer(stream, group, consumer);
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
    // Purge every PEL row owned by this consumer plus the registry row,
    // through the shared removal plan (the idle GC reuses it verbatim).
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
    let (batch, purged) = plan_consumer_removals(&prefix, &stream, &group, &[&consumer], &rows);
    if let Err(e) = ctx.commit(batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xgroup failed: {e}"));
    }
    consumer_removal_effects(ctx.shared, &stream, &group, &consumer, purged);
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
