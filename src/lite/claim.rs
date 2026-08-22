//! XCLAIM: re-assign pending entries across consumers (stuck-consumer
//! take-over, crash recovery). The claim rewrites PEL rows under the
//! stream latch in ONE commit batch: a claim moves ownership, refreshes
//! the delivery clock and bumps the delivery count (pending/backlog
//! totals do not change, so no `bump_pending`). A claimed entry whose
//! log record was trimmed away is dropped from the PEL instead of being
//! handed out with no payload. XAUTOCLAIM lives in [`super::autoclaim`]
//! and shares this file's PEL-rewrite helpers.
//!
//! Lite subset of the Redis flags (anything else is "ERR syntax error"):
//! XCLAIM takes JUSTID and FORCE -- full Redis's TIME / RETRYCOUNT /
//! IDLE delivery hints are not carried by the PEL record.

use crate::command::Ctx;
use crate::ds::expire;
use crate::ds::latch;
use crate::resp::codec as resp;
use crate::store::ops;

use super::entries;
use super::model::{self, EntryId};
use super::offset;
use super::pel;

/// NOGROUP reply. Byte-identical twin of read.rs's private helper:
/// widening THAT one would couple every PEL command to read.rs, so
/// read.rs keeps its own copy; the claim-family files (this one and
/// [`super::autoclaim`]) share this one.
pub(crate) fn nogroup(out: &mut Vec<u8>, stream: &[u8], group: &[u8]) {
    resp::append_error(
        out,
        &format!(
            "NOGROUP No such key '{}' or consumer group '{}'",
            String::from_utf8_lossy(stream),
            String::from_utf8_lossy(group)
        ),
    );
}

/// Decimal u64 option value (`<min-idle-time>`, `COUNT <n>`).
pub(crate) fn parse_u64(s: &[u8]) -> Option<u64> {
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Group existence check. A store error reads as absent (same tradeoff
/// as read.rs/ack.rs: surface errors from the claim reads themselves).
pub(crate) fn group_absent(ctx: &Ctx<'_>, prefix: &[u8], stream: &[u8], group: &[u8]) -> bool {
    offset::load(
        &ctx.shared.lite.offsets,
        &ctx.shared.store,
        prefix,
        stream,
        group,
    )
    .ok()
    .flatten()
    .is_none()
}

/// Entry field/value pairs read from the log.
pub(crate) type Fields = Vec<(Vec<u8>, Vec<u8>)>;

/// Smallest id strictly greater than `id` (cursor successor); the MAX
/// fallback only matters at the u64 id ceiling, where nothing follows.
pub(crate) fn succ_id(id: EntryId) -> Option<EntryId> {
    if id.seq < u64::MAX {
        Some(EntryId {
            seq: id.seq + 1,
            ..id
        })
    } else if id.ms < u64::MAX {
        Some(EntryId {
            ms: id.ms + 1,
            seq: 0,
        })
    } else {
        None
    }
}

/// Point read of one entry's field list; `None` = trimmed or deleted.
pub(crate) fn read_entry(
    store: &crate::store::Store,
    prefix: &[u8],
    stream: &[u8],
    id: EntryId,
) -> Result<Option<Fields>, String> {
    ops::get_physical(store, &model::entry_key(prefix, stream, id))
        .map(|v| v.and_then(|raw| model::decode_entry(&raw)))
}

/// The claimed-row rewrite: ownership moves to `consumer` and the
/// delivery clock refreshes; `times_delivered` bumps unless the claim is
/// JUSTID-only (an ownership move is not a delivery, the old count
/// stays). FORCE-created rows have no prior count and start at 1.
pub(crate) fn claimed_state(
    old_times: u64,
    fresh: bool,
    justid: bool,
    consumer: &[u8],
    now: u64,
) -> pel::PendState {
    pel::PendState {
        consumer: consumer.to_vec(),
        delivered_ms: now,
        times_delivered: match (fresh, justid) {
            (true, _) => 1,
            (false, false) => old_times + 1,
            (false, true) => old_times,
        },
    }
}

/// Register the claiming consumer: the runtime registry answers in
/// memory; `false` = first sighting, so the consumer's persisted record
/// rides the caller's claim batch (restarts keep XINFO CONSUMERS whole).
pub(crate) fn register_consumer(
    ctx: &Ctx<'_>,
    batch: &mut rocksdb::WriteBatch,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
    consumer: &[u8],
    now: u64,
) {
    if !ctx.shared.lite.ensure_consumer(stream, group, consumer) {
        batch.put(
            pel::consumer_key(prefix, stream, group, consumer),
            pel::encode_consumer(&pel::ConsumerState { created_ms: now }),
        );
    }
}

/// `XCLAIM <stream> <group> <consumer> <min-idle-time> <id> [id ...] [JUSTID] [FORCE]`.
pub async fn xclaim(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 5 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xclaim' command",
        );
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, 0) else {
        return;
    };
    let (group, consumer) = (ctx.args[1].clone(), ctx.args[2].clone());
    let Some(min_idle) = parse_u64(&ctx.args[3]) else {
        return resp::append_error(ctx.out, "ERR value is not an integer or out of range");
    };
    // Lite flags interleaved with the id list; every other token must be
    // a plain id (TIME/RETRYCOUNT/IDLE & co. are not supported here).
    let (mut justid, mut force) = (false, false);
    let mut ids = Vec::with_capacity(ctx.args.len() - 4);
    for a in &ctx.args[4..] {
        if a.eq_ignore_ascii_case(b"JUSTID") {
            justid = true;
        } else if a.eq_ignore_ascii_case(b"FORCE") {
            force = true;
        } else {
            match model::parse_id(a) {
                Some(id) => ids.push(id),
                None => return resp::append_error(ctx.out, "ERR Invalid stream ID specified"),
            }
        }
    }
    if group_absent(ctx, &prefix, &stream, &group) {
        return nogroup(ctx.out, &stream, &group);
    }
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    // Re-validate under the latch: a racing XGROUP DESTROY may have
    // removed the group between the first check and the latch.
    if group_absent(ctx, &prefix, &stream, &group) {
        return nogroup(ctx.out, &stream, &group);
    }
    let now = expire::now_ms();
    let mut batch = rocksdb::WriteBatch::default();
    register_consumer(ctx, &mut batch, &prefix, &stream, &group, &consumer, now);
    let mut frames: Vec<entries::Entry> = Vec::new();
    let mut claimed_ids: Vec<EntryId> = Vec::new();
    // FORCE can MINT a PEL row for an id that was never delivered: the
    // backlog counter only grows for those (rewrites are count-neutral).
    let mut force_created: u64 = 0;
    for id in ids {
        let old = match pel::get_pend(&ctx.shared.store, &prefix, &stream, &group, id) {
            // Not idle enough yet: stays with its current owner.
            Ok(Some(st)) if now.saturating_sub(st.delivered_ms) < min_idle => continue,
            Ok(st) => st,
            Err(e) => return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}")),
        };
        let fresh = old.is_none();
        if fresh && !force {
            continue; // unknown id without FORCE: nothing pending, nothing to claim
        }
        // The log read gates FORCE (the entry must exist) and feeds the
        // full-form reply; a JUSTID claim of a known row skips it -- a
        // trimmed entry can still change hands, only its payload is gone.
        let fields = if justid && !fresh {
            None
        } else {
            match read_entry(&ctx.shared.store, &prefix, &stream, id) {
                Ok(v) => v,
                Err(e) => return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}")),
            }
        };
        if fresh && fields.is_none() {
            continue; // FORCE on a trimmed id: no entry to claim
        }
        if !justid && fields.is_none() {
            // Delivered entry trimmed from the log: drop the orphan PEL
            // row instead of redelivering a missing payload.
            batch.delete(pel::pend_key(&prefix, &stream, &group, id));
            continue;
        }
        if fresh {
            force_created += 1;
        }
        batch.put(
            pel::pend_key(&prefix, &stream, &group, id),
            pel::encode_pend(&claimed_state(
                old.map_or(0, |st| st.times_delivered),
                fresh,
                justid,
                &consumer,
                now,
            )),
        );
        match fields {
            Some(fields) if !justid => frames.push(entries::Entry { id, fields }),
            _ => claimed_ids.push(id),
        }
    }
    if let Err(e) = ctx.commit(batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}"));
    }
    if force_created > 0 {
        offset::bump_pending(
            &ctx.shared.lite.offsets,
            &stream,
            &group,
            force_created as i64,
        );
    }
    // Only entries actually claimed, in argument order; none -> *0.
    if justid {
        resp::append_array(ctx.out, claimed_ids.len());
        for id in &claimed_ids {
            resp::append_bulk(ctx.out, model::format_id(*id).as_bytes());
        }
    } else {
        resp::append_array(ctx.out, frames.len());
        for e in &frames {
            entries::append_entry_frame(ctx.out, e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claimed_state_bumps_unless_justid_and_starts_fresh_at_one() {
        let st = claimed_state(4, false, false, b"c2", 1234);
        assert_eq!(st.consumer, b"c2".to_vec());
        assert_eq!(st.delivered_ms, 1234);
        assert_eq!(st.times_delivered, 5); // plain claim: 4 -> 5
                                           // JUSTID moves ownership only: count and delivery stay untouched.
        let st = claimed_state(4, false, true, b"c2", 1234);
        assert_eq!(st.times_delivered, 4);
        assert_eq!(st.delivered_ms, 1234);
        // FORCE-created rows have no prior count: always 1, JUSTID or not.
        assert_eq!(claimed_state(0, true, false, b"c2", 1).times_delivered, 1);
        assert_eq!(claimed_state(0, true, true, b"c2", 1).times_delivered, 1);
    }
}
