//! Dead-letter transfers (XGROUP MAXDELIVERY/DLQ): the poison-message
//! escape hatch. A group configured with `MAXDELIVERY <n>` dead-letters
//! a pending row whose NEXT delivery would push `times_delivered` past
//! `n`: the entry is re-queued into the DLQ stream (original id/fields
//! + trace fields), the PEL row is dropped and the committed watermark
//!   advances over the resolved id -- ONE WriteBatch the caller commits
//!   under the source stream latch (plus the sorted DLQ latch), so a
//!   transfer is semantically an ack: XPENDING stops seeing the row and a
//!   restart never resurrects it. Triggers live on the delivery paths
//!   (XREADGROUP re-delivery, XCLAIM/XAUTOCLAIM, the idle sweep), never a
//!   background scanner, so judgment and transfer serialize inside one
//!   latch; racing claimers cannot double-transfer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

use rocksdb::WriteBatch;

use crate::ds::{codec, wait};
use crate::state;
use crate::store::ops;

use super::model::{self, EntryId, MetaPayload};
use super::offset::{self, GroupState};
use super::pel;

/// Trace fields appended to a dead-lettered entry; a colliding payload
/// name keeps its value verbatim.
pub const F_GROUP: &[u8] = b"__dlq_group";
pub const F_CONSUMER: &[u8] = b"__dlq_consumer";
pub const F_TIMES: &[u8] = b"__dlq_times";
pub const F_SRC: &[u8] = b"__dlq_src";

/// NEXT delivery (`old_times + 1`) past the cap? cap 0 never trips;
/// count 0 never exceeds cap >= 1.
pub(crate) fn should_dead_letter(old_times: u64, maxdelivery: u64) -> bool {
    maxdelivery > 0 && old_times + 1 > maxdelivery
}

/// Split a scanned delivery batch into its dead-letter candidates: ids
/// whose CURRENT pending count (0 = never delivered, which can never
/// trip a cap >= 1) would pass the group's cap on the next hand-out.
pub(crate) fn split_dead(
    pairs: impl Iterator<Item = (EntryId, u64)>,
    maxdelivery: u64,
    consumer: &[u8],
) -> Vec<DeadRow> {
    pairs
        .filter(|(_, old)| should_dead_letter(*old, maxdelivery))
        .map(|(id, times)| DeadRow {
            id,
            times,
            consumer: consumer.to_vec(),
        })
        .collect()
}

/// Default DLQ target of a MAXDELIVERY group created without an
/// explicit `DLQ` name: the literal `<stream>/dlq`. The three-part
/// name keeps the PARENT topic (`t/q0/dlq` -> parent `t`;
/// stream_prefix slots by the bytes before the FIRST '/'), so the
/// target lands in the SAME SLOT -- one contiguous window; nested
/// children are valid names and excluded from XADD parent picks, so a
/// DLQ never receives round-robin traffic.
pub(crate) fn default_dlq_stream(stream: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(stream.len() + 5);
    name.extend_from_slice(stream);
    name.extend_from_slice(b"/dlq");
    name
}

/// Meta latch key of a DLQ target under its own slot prefix (`None` if no valid slot layout).
pub(crate) fn dlq_latch_key(dlq_stream: &[u8]) -> Option<Vec<u8>> {
    model::stream_prefix(dlq_stream).map(|p| model::meta_key(&p, dlq_stream))
}

/// Sorted, deduped latch keys of a source stream plus its group's DLQ
/// target when configured (multi-key lockers take byte order, the
/// deadlock convention); unconfigured groups pay zero extra keys.
pub(crate) fn latch_keys(stream_key: Vec<u8>, dlq_stream: Option<&[u8]>) -> Vec<Vec<u8>> {
    let mut keys = vec![stream_key];
    if let Some(k) = dlq_stream.and_then(dlq_latch_key) {
        keys.push(k);
    }
    keys.sort();
    keys.dedup();
    keys
}

/// The group's DLQ target iff its immutable CREATE config asks for
/// transfers (maxdelivery > 0, target resolved): `None` otherwise.
pub(crate) fn group_dlq_target(st: Option<&GroupState>) -> Option<Vec<u8>> {
    st.filter(|st| st.maxdelivery > 0 && !st.dlq.is_empty())
        .map(|st| st.dlq.clone())
}

/// Latch acquisition with drift re-validation for ONE group: peek the
/// CREATE config, lock [`latch_keys`] (byte-sorted), then re-peek
/// UNDER the latches -- a DESTROY + re-CREATE with a DIFFERENT DLQ can
/// commit between peek and lock, and writing the new target's window
/// without its latch would race every DLQ writer on it. A drifted set
/// is dropped and re-taken; the loop converges because the config is
/// frozen while the stream latch is held (CREATE/DESTROY take it).
pub(crate) async fn lock_group_latches(
    hub: &crate::ds::latch::Latch,
    cache: &super::offset::OffsetCache,
    store: &crate::store::Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> (
    Vec<crate::ds::latch::KeyGuard>,
    Option<super::offset::GroupState>,
) {
    loop {
        let gst = offset::load(cache, store, prefix, stream, group)
            .ok()
            .flatten();
        let target = group_dlq_target(gst.as_ref());
        let set = latch_keys(model::meta_key(prefix, stream), target.as_deref());
        let mut guards = Vec::with_capacity(set.len());
        for k in &set {
            guards.push(crate::ds::latch::lock(hub, k).await);
        }
        // Re-validation under the lock: a still-matching target means
        // the held set is exactly the config's own.
        if group_dlq_target(offset::peek_cached(cache, stream, group).as_ref()) == target {
            return (guards, gst);
        }
        drop(guards); // stale set: re-peek and re-lock
    }
}

/// One dead-letter candidate: the pending row id, its CURRENT delivery
/// count (pre-bump, goes into `__dlq_times`) and the consumer the hand-
/// out would have gone to (`__dlq_consumer`: the claiming reader, or
/// the row's own owner on the sweep path).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeadRow {
    pub id: EntryId,
    pub times: u64,
    pub consumer: Vec<u8>,
}

/// Latched transfer context: the caller's open batch, source stream placement, group + state.
pub(crate) struct TransferCtx<'a> {
    pub shared: &'a state::Shared,
    pub batch: &'a mut WriteBatch,
    pub prefix: &'a [u8],
    pub stream: &'a [u8],
    pub group: &'a [u8],
    pub st: &'a GroupState,
}

pub(crate) fn transfer_ctx<'a>(
    shared: &'a state::Shared,
    batch: &'a mut WriteBatch,
    prefix: &'a [u8],
    stream: &'a [u8],
    group: &'a [u8],
    st: &'a GroupState,
) -> TransferCtx<'a> {
    TransferCtx {
        shared,
        batch,
        prefix,
        stream,
        group,
        st,
    }
}

/// XREADGROUP `>` delivery gate: entries whose NEXT re-delivery would
/// pass the group's cap are transferred on the caller's batch and
/// dropped from the served set; the cached pending backlog moves with
/// them (+fresh kept, -transferred). Returns kept entries, the
/// [`TransferTally`] (accounted by the caller after its commit) and
/// the [`offset::RollbackMark`] captured before any mutation -- the
/// caller restores it when the commit FAILS, so the watermark never
/// names ids the store never resolved. Fresh rows (count 0) never
/// trip a cap >= 1; ordered groups only re-deliver the head, and a
/// head transfer frees the queue.
pub(crate) fn gate_delivery(
    ctx: TransferCtx<'_>,
    entries: Vec<super::entries::Entry>,
    pending: &HashMap<EntryId, u64>,
    consumer: &[u8],
    now_ms: u64,
) -> Result<
    (
        Vec<super::entries::Entry>,
        TransferTally,
        Option<offset::RollbackMark>,
    ),
    String,
> {
    let mark = offset::mark(&ctx.shared.lite.offsets, ctx.stream, ctx.group);
    let dead = if ctx.st.maxdelivery == 0 || pending.is_empty() {
        Vec::new()
    } else {
        split_dead(
            entries
                .iter()
                .map(|e| (e.id, pending.get(&e.id).copied().unwrap_or(0))),
            ctx.st.maxdelivery,
            consumer,
        )
    };
    let kept: Vec<super::entries::Entry> = entries
        .into_iter()
        .filter(|e| !dead.iter().any(|d| d.id == e.id))
        .collect();
    let fresh = kept.iter().filter(|e| !pending.contains_key(&e.id)).count() as i64;
    let (shared, stream, group) = (ctx.shared, ctx.stream, ctx.group);
    let tally = if dead.is_empty() {
        TransferTally::default()
    } else {
        transfer_entries(ctx, &dead, now_ms)?
    };
    offset::bump_pending(
        &shared.lite.offsets,
        stream,
        group,
        fresh - tally.resolved as i64,
    );
    Ok((kept, tally, mark))
}

/// GroupPayload snapshot of a cached state (the shared ack-path shape).
pub(crate) fn payload_of(st: &GroupState) -> model::GroupPayload {
    model::GroupPayload {
        created_ms: st.created_ms,
        delivered_ms: st.delivered.ms,
        delivered_seq: st.delivered.seq,
        committed_ms: st.committed.ms,
        committed_seq: st.committed.seq,
        ordered: st.ordered,
        inflight_max: st.inflight_max,
        maxdelivery: st.maxdelivery,
        dlq: st.dlq.clone(),
    }
}

/// Append trace fields to `fields`; a name already in the payload
/// keeps its value verbatim.
pub(crate) fn append_trace(
    fields: &mut Vec<(Vec<u8>, Vec<u8>)>,
    group: &[u8],
    consumer: &[u8],
    times: u64,
    src: &[u8],
) {
    let mut push = |f: &[u8], v: Vec<u8>| {
        if !fields.iter().any(|(name, _)| name == f) {
            fields.push((f.to_vec(), v));
        }
    };
    push(F_GROUP, group.to_vec());
    push(F_CONSUMER, consumer.to_vec());
    push(F_TIMES, times.to_string().into_bytes());
    push(F_SRC, src.to_vec());
}

/// Point meta read without read_meta's lazy purge (transfers are latched); corrupt = missing.
pub(crate) fn read_dlq_meta(
    shared: &state::Shared,
    prefix: &[u8],
    stream: &[u8],
) -> Result<Option<MetaPayload>, String> {
    Ok(
        ops::get_physical(&shared.store, &model::meta_key(prefix, stream))?.and_then(|raw| {
            let (_, body) = codec::decode_envelope(&raw);
            serde_json::from_slice::<MetaPayload>(body).ok()
        }),
    )
}

/// Dead-letter `dead` rows onto the caller's OPEN batch (callers MUST
/// hold the [`latch_keys`] latches across the write and its commit).
/// Per row: a live source entry is re-queued into the DLQ stream at its
/// ORIGINAL id (trace fields appended; DLQ meta lazily created, `len`
/// bumped, `last` = max id); a trimmed/orphan row transfers nothing.
/// Either way the PEL row is deleted -- RESOLVED. The committed
/// watermark folds the ids in as acked (`offset::resolve` +
/// `pel::head_after_ack`) and, when it moved, the group record rides
/// the same batch (the crash resume point must never sit before a
/// transferred id). Returns the [`TransferTally`] the caller applies
/// only AFTER its commit SUCCEEDS (accounting, never the batch).
pub(crate) fn transfer_entries(
    ctx: TransferCtx<'_>,
    dead: &[DeadRow],
    now_ms: u64,
) -> Result<TransferTally, String> {
    let (shared, batch, st) = (ctx.shared, ctx.batch, ctx.st);
    let (prefix, stream, group) = (ctx.prefix, ctx.stream, ctx.group);
    if dead.is_empty() {
        return Ok(TransferTally::default());
    }
    let dlq_stream = st.dlq.clone();
    let dlq_prefix =
        model::stream_prefix(&dlq_stream).ok_or_else(|| "DLQ target has no slot".to_string())?;
    let existing = read_dlq_meta(shared, &dlq_prefix, &dlq_stream)?;
    let fresh = existing.is_none();
    let mut meta = existing.unwrap_or_default();
    let mut appended = 0usize;
    let mut last_id = meta.last_id();
    for row in dead {
        if let Some(fields) =
            ops::get_physical(&shared.store, &model::entry_key(prefix, stream, row.id))?
                .and_then(|raw| model::decode_entry(&raw))
        {
            let dkey = model::entry_key(&dlq_prefix, &dlq_stream, row.id);
            // Shared-DLQ probe: a SECOND group re-queueing the same id
            // overwrites the target entry (one key), so it must not
            // inflate `len` a second time; a probe error keeps the
            // count-it behavior rather than failing the transfer.
            let known = ops::get_physical(&shared.store, &dkey)
                .ok()
                .flatten()
                .is_some();
            let mut pairs = fields;
            append_trace(&mut pairs, group, &row.consumer, row.times, stream);
            let refs: Vec<(&[u8], &[u8])> = pairs
                .iter()
                .map(|(f, v)| (f.as_slice(), v.as_slice()))
                .collect();
            batch.put(&dkey, model::encode_entry(&refs));
            last_id = last_id.max(row.id);
            if !known {
                meta.len += 1;
            }
            appended += 1;
        }
        batch.delete(pel::pend_key(prefix, stream, group, row.id));
    }
    if appended > 0 {
        if fresh {
            meta.created_ms = now_ms;
        }
        meta.last_ms = last_id.ms;
        meta.last_seq = last_id.seq;
        // Same idle-deadline retouch as XADD (idle = no writes), with
        // the id-ms floor; saturate instead of failing the transfer.
        let expire = if meta.idle_ms > 0 {
            now_ms.max(last_id.ms).saturating_add(meta.idle_ms)
        } else {
            0
        };
        batch.put(
            model::meta_key(&dlq_prefix, &dlq_stream),
            model::encode_meta_at(&meta, expire),
        );
    }
    // Resolved ids count as acked, and the watermark may continue over
    // ids resolved by EARLIER batches (rows already gone): the walk
    // runs to the delivery bound, so draining the last gap lands the
    // committed mark on the stream position instead of parking on the
    // last transferred id. It still never crosses a LIVE pending row.
    let ids: Vec<EntryId> = dead.iter().map(|r| r.id).collect();
    let bound = st
        .delivered
        .max(ids.iter().copied().max().unwrap_or(st.committed));
    let head_after = if bound > st.committed {
        let acked: HashSet<EntryId> = ids.iter().copied().collect();
        pel::head_after_ack(
            &shared.store,
            prefix,
            stream,
            group,
            st.committed,
            &acked,
            bound,
        )
        .ok()
        .flatten()
    } else {
        None
    };
    offset::resolve(&shared.lite.offsets, stream, group, &ids, head_after, bound);
    let st_now =
        offset::peek_cached(&shared.lite.offsets, stream, group).unwrap_or_else(|| st.clone());
    if st_now.committed > st.committed {
        batch.put(
            model::group_key(prefix, stream, group),
            model::encode_group(&payload_of(&st_now)),
        );
    }
    Ok(TransferTally {
        resolved: dead.len(),
        moved: appended,
        dlq_created: usize::from(fresh && appended > 0),
    })
}

/// What one transfer batch did, for the caller to account only after
/// its commit SUCCEEDS: `resolved` PEL rows left the group, `moved`
/// entries actually re-queued into the DLQ (trimmed/orphans transfer
/// nothing), and `dlq_created` counts DLQ stream metas being
/// lazily created by this batch (the live-streams gauge must not see a
/// stream whose creation write never landed).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TransferTally {
    pub resolved: usize,
    pub moved: usize,
    pub dlq_created: usize,
}

/// Apply a committed batch's [`TransferTally`]: message counter and,
/// for a lazily created DLQ stream, the live-streams gauge.
pub(crate) fn account_tally(shared: &state::Shared, tally: TransferTally) {
    if tally.dlq_created > 0 {
        shared
            .lite
            .stats
            .streams_live
            .fetch_add(tally.dlq_created as i64, Ordering::Relaxed);
    }
    if tally.moved > 0 {
        super::stat_bump(&shared.lite.stats.messages, tally.moved as u64);
    }
}

/// Wake the readers a transfer concerns: source stream (slots freed) and DLQ stream (new entry).
pub(crate) fn notify_transfer(
    shared: &state::Shared,
    prefix: &[u8],
    stream: &[u8],
    dlq_stream: &[u8],
) {
    wait::notify(&shared.wait_hub, &model::meta_key(prefix, stream));
    if let Some(k) = dlq_latch_key(dlq_stream) {
        wait::notify(&shared.wait_hub, &k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_dead_letter_boundaries() {
        // Unset cap never trips; a never-delivered row cannot exceed any cap >= 1.
        assert!(!should_dead_letter(99, 0));
        assert!(!should_dead_letter(0, 3));
        // Cap 1 trips on the row's first RE-delivery.
        assert!(should_dead_letter(1, 1));
        // Exact boundary: old_times + 1 == max stays WITHIN the cap.
        assert!(!should_dead_letter(2, 3));
        assert!(should_dead_letter(3, 3));
        // split_dead (consumer-stamped candidates) rides this predicate; see redeliver's sweep test.
    }

    #[test]
    fn naming_and_latch_key_helpers() {
        // `orders/q0` -> `orders/q0/dlq`: valid nested name, same PARENT slot.
        let d = default_dlq_stream(b"orders/q0");
        assert_eq!(d, b"orders/q0/dlq".to_vec());
        assert_eq!(model::stream_prefix(&d), model::stream_prefix(b"orders/q0"));
        assert!(crate::lite::parse_topic_name(&d).is_ok());
        // Latch keys: the REAL parent slot of `t` (stream_latch_key convention);
        // zero extra keys unconfigured; a self-pointing DLQ dedupes; a second sorts.
        let src = model::meta_key(&crate::hash::slot_with_prefix(b"t").1, b"t/zz");
        assert_eq!(latch_keys(src.clone(), None), vec![src.clone()]);
        assert_eq!(latch_keys(src.clone(), Some(b"t/zz")), vec![src.clone()]);
        let keys = latch_keys(src, Some(b"t/dlq_zz"));
        assert_eq!(keys.len(), 2, "DLQ adds its latch");
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "byte-sorted");
    }

    #[test]
    fn append_trace_skips_colliding_names() {
        let mut fields = vec![(F_SRC.to_vec(), b"keep".to_vec())];
        append_trace(&mut fields, b"g", b"c1", 3, b"t/q0");
        // The colliding __dlq_src keeps its value; the other three ride along.
        assert_eq!(fields[0].1, b"keep".to_vec());
        assert_eq!(fields.len(), 4);
        assert!(fields.iter().any(|(f, v)| f == F_TIMES && v == b"3"));
    }
}
