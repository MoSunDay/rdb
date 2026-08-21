//! XACK: advance a consumer group's committed watermark and drop the
//! acked entries' PEL rows. The committed watermark is the restart
//! resume point, so both go to disk in one synchronous latched batch
//! instead of waiting for the 200ms flusher. The reply counts watermark
//! advancement (Lite semantics), not pending rows removed.

use crate::command::Ctx;
use crate::ds::latch;
use crate::monitor;
use crate::resp::codec as resp;

use super::entries;
use super::model;
use super::offset;
use super::stat_bump;

/// `XACK <stream> <group> <id> [id ...]`: advance the committed watermark
/// to the max acked id; un-acked messages are redelivered after a restart.
pub async fn xack(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 3 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xack' command");
    }
    let mut ids = Vec::with_capacity(ctx.args.len() - 2);
    for a in &ctx.args[2..] {
        match model::parse_id(a) {
            Some(id) => ids.push(id),
            None => {
                return resp::append_error(
                    ctx.out,
                    "ERR Invalid stream ID specified as stream command argument",
                )
            }
        }
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, 0) else {
        return;
    };
    // Group names are raw bytes (no charset validation at this layer):
    // pass them through undecoded so distinct byte names stay distinct
    // cache keys (a lossy String key would merge them into U+FFFD).
    let group = ctx.args[1].clone();
    let known = offset::load(
        &ctx.shared.lite.offsets,
        &ctx.shared.store,
        &prefix,
        &stream,
        &group,
    )
    .ok()
    .flatten()
    .is_some();
    let count = if known {
        let n = offset::ack(&ctx.shared.lite.offsets, &stream, &group, &ids).unwrap_or(0);
        // PEL rows go away with their ack: point-check every id first
        // (reads need no latch), then one latched batch deletes them
        // atomically with the watermark persist below. Ids at/below the
        // watermark (SETID rewinds, reclaimed redeliveries) can still be
        // pending, so the deletion is not gated on `n > 0`.
        let pend_hits: Vec<model::EntryId> = ids
            .iter()
            .filter(|id| {
                super::pel::get_pend(&ctx.shared.store, &prefix, &stream, &group, **id)
                    .ok()
                    .flatten()
                    .is_some()
            })
            .copied()
            .collect();
        // The committed watermark is the restart resume point: persist it
        // synchronously so acks survive kill -9 between flush rounds (the
        // 200ms flusher then only covers the delivered watermark).
        if n > 0 || !pend_hits.is_empty() {
            let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
            if let Some(st) = offset::load(
                &ctx.shared.lite.offsets,
                &ctx.shared.store,
                &prefix,
                &stream,
                &group,
            )
            .ok()
            .flatten()
            {
                let mut batch = rocksdb::WriteBatch::default();
                if n > 0 {
                    batch.put(
                        model::group_key(&prefix, &stream, group.as_slice()),
                        model::encode_group(&model::GroupPayload {
                            created_ms: st.created_ms,
                            delivered_ms: st.committed.ms,
                            delivered_seq: st.committed.seq,
                            committed_ms: st.committed.ms,
                            committed_seq: st.committed.seq,
                        }),
                    );
                }
                for id in &pend_hits {
                    batch.delete(super::pel::pend_key(&prefix, &stream, &group, *id));
                }
                if let Err(e) = ctx.commit(batch).await {
                    return resp::append_error(ctx.out, &format!("ERR: xack failed: {e}"));
                }
                offset::bump_pending(
                    &ctx.shared.lite.offsets,
                    &stream,
                    &group,
                    -(pend_hits.len() as i64),
                );
            }
        }
        n
    } else {
        0
    };
    stat_bump(&ctx.shared.lite.stats.acks, count as u64);
    monitor::observe_lite_message(&ctx.shared.monitor, "ack", count as u64);
    resp::append_int(ctx.out, count as i64);
}
