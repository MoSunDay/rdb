//! XACK: advance a consumer group's committed watermark (Kafka
//! committed-offset semantics: a POSITION that only moves over a
//! CONTIGUOUS acked prefix -- `pel::head_after_ack` probes the first
//! surviving pending row and the watermark stops below it, so a restart
//! can never resume past an unacked entry) and drop the acked entries'
//! PEL rows. The committed watermark is the restart resume point, so
//! both go to disk in one synchronous latched batch instead of waiting
//! for the 200ms flusher. The reply counts acked ids beyond the old
//! watermark (Lite semantics), not pending rows removed.

use crate::command::Ctx;
use crate::ds::latch;
use crate::monitor;
use crate::resp::codec as resp;

use super::entries;
use super::model;
use super::offset;
use super::stat_bump;

/// `XACK <stream> <group> <id> [id ...]`: acked ids beyond a pending
/// gap stay acked on the PEL side but do not advance the position --
/// the tail is redelivered later (at-least-once duplicates, never loss).
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
        // The whole ack serializes against deliveries on the stream
        // latch: the gap probe must not miss a row a concurrent
        // delivery is about to write, or the watermark could skip it.
        let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
        // Re-validate under the latch: a racing XGROUP DESTROY may have
        // removed the group between the first check and the latch.
        let Some(st0) = offset::load(
            &ctx.shared.lite.offsets,
            &ctx.shared.store,
            &prefix,
            &stream,
            &group,
        )
        .ok()
        .flatten() else {
            return resp::append_int(ctx.out, 0);
        };
        let old_committed = st0.committed;
        // PEL rows go away with their ack: point-check every id first.
        // Ids at/below the watermark (SETID rewinds, reclaimed
        // redeliveries) can still be pending, so the deletion is not
        // gated on the reply count.
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
        let max_acked = ids.iter().max().copied().unwrap_or(old_committed);
        // Contiguous-prefix probe: only meaningful when the ack reaches
        // past the old watermark at all.
        let head_after = if max_acked > old_committed {
            let acked: std::collections::HashSet<model::EntryId> = ids.iter().copied().collect();
            super::pel::head_after_ack(
                &ctx.shared.store,
                &prefix,
                &stream,
                &group,
                old_committed,
                &acked,
                max_acked,
            )
            .ok()
            .flatten()
        } else {
            None
        };
        let n =
            offset::ack(&ctx.shared.lite.offsets, &stream, &group, &ids, head_after).unwrap_or(0);
        let advanced = offset::load(
            &ctx.shared.lite.offsets,
            &ctx.shared.store,
            &prefix,
            &stream,
            &group,
        )
        .ok()
        .flatten()
        .is_some_and(|st| st.committed > old_committed);
        // The committed watermark is the restart resume point: persist
        // it synchronously so acks survive kill -9 between flush rounds
        // (the 200ms flusher then only covers the delivered watermark).
        if advanced || !pend_hits.is_empty() {
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
                if advanced {
                    batch.put(
                        model::group_key(&prefix, &stream, group.as_slice()),
                        model::encode_group(&model::GroupPayload {
                            created_ms: st.created_ms,
                            delivered_ms: st.committed.ms,
                            delivered_seq: st.committed.seq,
                            committed_ms: st.committed.ms,
                            committed_seq: st.committed.seq,
                            ordered: st.ordered,
                            inflight_max: st.inflight_max,
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
