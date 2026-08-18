//! XACK: advance a consumer group's committed watermark. The committed
//! watermark is the restart resume point, so it is persisted synchronously
//! (one latched batched fsync) instead of waiting for the 200ms flusher.

use std::sync::Arc;

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
    let group = String::from_utf8_lossy(&ctx.args[1]).to_string();
    let stream_str = String::from_utf8_lossy(&stream).to_string();
    let known = offset::load(
        &ctx.shared.lite.offsets,
        &ctx.shared.store,
        &prefix,
        &stream_str,
        &group,
    )
    .ok()
    .flatten()
    .is_some();
    let count = if known {
        let n = offset::ack(&ctx.shared.lite.offsets, &stream_str, &group, &ids).unwrap_or(0);
        // The committed watermark is the restart resume point: persist it
        // synchronously so acks survive kill -9 between flush rounds (the
        // 200ms flusher then only covers the delivered watermark).
        if n > 0 {
            let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
            if let Some(st) = offset::load(
                &ctx.shared.lite.offsets,
                &ctx.shared.store,
                &prefix,
                &stream_str,
                &group,
            )
            .ok()
            .flatten()
            {
                let mut batch = rocksdb::WriteBatch::default();
                batch.put(
                    model::group_key(&prefix, &stream, ctx.args[1].as_slice()),
                    model::encode_group(&model::GroupPayload {
                        created_ms: st.created_ms,
                        delivered_ms: st.committed.ms,
                        delivered_seq: st.committed.seq,
                        committed_ms: st.committed.ms,
                        committed_seq: st.committed.seq,
                    }),
                );
                if let Err(e) =
                    crate::store::ops::batch_write_async(Arc::clone(&ctx.shared.store), batch).await
                {
                    return resp::append_error(ctx.out, &format!("ERR: xack failed: {e}"));
                }
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
