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
//! XCLAIM takes JUSTID, FORCE and the delivery hints IDLE / TIME /
//! RETRYCOUNT (any order, interleaved with the id list like Redis).
//! The hints rewrite the claimed PEL rows' delivery clock / counter
//! (JUSTID claims carry them too -- an ownership move is still a PEL
//! write); XPENDING's idle/deliveries columns and the idle sweep read
//! the same `delivered_ms` field, so a backdated claim (large IDLE or
//! a past TIME) becomes min-idle eligible that much sooner.

use crate::command::Ctx;
use crate::ds::expire;
use crate::monitor;
use crate::resp::codec as resp;
use crate::store::ops;

use super::dlq;
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

/// Where a claim's IDLE/TIME hint parks the delivery clock. The PEL's
/// `delivered_ms` stays the single source of truth for every idle
/// computation (XPENDING columns, the min-idle gate, the redelivery
/// sweep): the hint only chooses WHICH timestamp lands there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryAt {
    /// `IDLE <ms>`: delivered_ms = now - idle (backdated).
    Idle(u64),
    /// `TIME <unix-ms>`: delivered_ms = the given wall clock.
    Time(u64),
}

/// XCLAIM's delivery-hint overrides, applied to every successfully
/// claimed row: `delivery` (last-specified of IDLE/TIME wins, like
/// Redis) and `retrycount` (replaces the delivery counter outright).
#[derive(Default)]
pub(crate) struct ClaimHints {
    pub delivery: Option<DeliveryAt>,
    pub retrycount: Option<u64>,
}

/// The claimed-row rewrite: ownership moves to `consumer` and the
/// delivery clock refreshes; `times_delivered` bumps unless the claim is
/// JUSTID-only (an ownership move is not a delivery, the old count
/// stays). FORCE-created rows have no prior count and start at 1.
/// `epoch` stamps the ordered-group ownership generation (0 = unordered).
/// Claim hints (IDLE/TIME/RETRYCOUNT) override the clock/counter they
/// target; with no hint the historical behavior is byte-identical.
pub(crate) fn claimed_state(
    old_times: u64,
    fresh: bool,
    justid: bool,
    consumer: &[u8],
    now: u64,
    epoch: u64,
    hints: &ClaimHints,
) -> pel::PendState {
    pel::PendState {
        consumer: consumer.to_vec(),
        delivered_ms: match hints.delivery {
            Some(DeliveryAt::Idle(idle)) => now.saturating_sub(idle),
            Some(DeliveryAt::Time(t)) => t,
            None => now,
        },
        times_delivered: match hints.retrycount {
            Some(n) => n,
            None => match (fresh, justid) {
                (true, _) => 1,
                (false, false) => old_times + 1,
                (false, true) => old_times,
            },
        },
        epoch,
    }
}

/// Register the claiming consumer: the runtime registry answers in
/// memory; `None` = first sighting, so the consumer's persisted record
/// is created (`created_ms = now`), `Some(created)` rewrites it with a
/// fresh `seen_ms` -- either way the row rides the caller's claim batch
/// (restarts keep XINFO CONSUMERS whole; claims feed the idle-GC clock).
pub(crate) fn register_consumer(
    ctx: &Ctx<'_>,
    batch: &mut rocksdb::WriteBatch,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
    consumer: &[u8],
    now: u64,
) {
    let created = ctx
        .shared
        .lite
        .ensure_consumer(stream, group, consumer, now)
        .unwrap_or(now);
    batch.put(
        pel::consumer_key(prefix, stream, group, consumer),
        pel::encode_consumer(&pel::ConsumerState {
            created_ms: created,
            seen_ms: now,
        }),
    );
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
    // Flags and delivery hints interleave with the id list (Redis's own
    // grammar): JUSTID / FORCE are toggles, IDLE / TIME / RETRYCOUNT
    // take one integer value, everything else must be a plain id.
    let (mut justid, mut force) = (false, false);
    let mut hints = ClaimHints::default();
    let mut ids = Vec::with_capacity(ctx.args.len() - 4);
    let mut i = 4;
    while i < ctx.args.len() {
        let a = ctx.args[i].as_slice();
        if a.eq_ignore_ascii_case(b"JUSTID") {
            justid = true;
            i += 1;
        } else if a.eq_ignore_ascii_case(b"FORCE") {
            force = true;
            i += 1;
        } else if a.eq_ignore_ascii_case(b"IDLE") || a.eq_ignore_ascii_case(b"TIME") {
            let idle = a.eq_ignore_ascii_case(b"IDLE");
            let Some(v) = ctx.args.get(i + 1).and_then(|x| parse_u64(x)) else {
                return resp::append_error(ctx.out, "ERR value is not an integer or out of range");
            };
            hints.delivery = Some(if idle {
                DeliveryAt::Idle(v)
            } else {
                DeliveryAt::Time(v)
            });
            i += 2;
        } else if a.eq_ignore_ascii_case(b"RETRYCOUNT") {
            match ctx.args.get(i + 1).and_then(|x| parse_u64(x)) {
                Some(n) => hints.retrycount = Some(n),
                None => {
                    return resp::append_error(
                        ctx.out,
                        "ERR value is not an integer or out of range",
                    )
                }
            }
            i += 2;
        } else {
            match model::parse_id(a) {
                Some(id) => ids.push(id),
                None => return resp::append_error(ctx.out, "ERR Invalid stream ID specified"),
            }
            i += 1;
        }
    }
    if group_absent(ctx, &prefix, &stream, &group) {
        return nogroup(ctx.out, &stream, &group);
    }
    // A MAXDELIVERY group's DLQ target joins the latch set (sorted by
    // dlq::latch_keys): the dead-letter branch below writes both
    // windows inside this one critical section. The first group_absent
    // check warmed the offset cache, and lock_group_latches re-validates
    // the peeked set UNDER the locks (a DESTROY + re-CREATE with a
    // different DLQ must not race the latch choice). It returns the
    // config state of the final, validated acquisition.
    let (guards, gst) = dlq::lock_group_latches(
        &ctx.shared.latch,
        &ctx.shared.lite.offsets,
        &ctx.shared.store,
        &prefix,
        &stream,
        &group,
    )
    .await;
    let _guards = guards;
    // Pre-transfer cache snapshot: restored on every failed commit
    // below so the watermark never names ids the store never resolved.
    let mark = offset::mark(&ctx.shared.lite.offsets, &stream, &group);
    // Re-validate under the latch: a racing XGROUP DESTROY may have
    // removed the group between the first check and the latch.
    if group_absent(ctx, &prefix, &stream, &group) {
        return nogroup(ctx.out, &stream, &group);
    }
    let now = expire::now_ms();
    // Ordered groups: takeover is QUEUE-granular -- only the PEL HEAD is
    // claimable (there is no taking half a batch from under the owner;
    // deeper rows free up as the new owner works down the head). FORCE
    // minting beyond the head would break log order, so it is suppressed
    // too (non-head ids are silently ignored, like Redis's non-pending
    // ones).
    let mut epoch = 0u64;
    let ordered = gst.as_ref().is_some_and(|st| st.ordered);
    if ordered {
        // The head row must exist, be the claimed id, and be idle
        // enough (FORCE does not bypass a pending row's idle gate);
        // otherwise the claim is empty and NO takeover happens.
        let head = pel::scan_pend(
            &ctx.shared.store,
            &prefix,
            &stream,
            &group,
            model::MIN_ID,
            Some(1),
        )
        .ok()
        .and_then(|rows| rows.into_iter().next());
        let claimable = head.as_ref().is_some_and(|row| {
            ids.contains(&row.id) && now.saturating_sub(row.state.delivered_ms) >= min_idle
        });
        if claimable {
            ids.retain(|id| Some(*id) == head.as_ref().map(|row| row.id));
            // The claiming consumer is the new owner from here on: bump
            // the generation so the deposed owner's later `>` reads
            // deliver nothing (zombie fencing, Kafka-generation style).
            epoch = super::ordered::force_takeover(
                &ctx.shared.lite.owners,
                &stream,
                &group,
                &consumer,
                now,
            );
        } else {
            ids.clear();
        }
    }
    let mut batch = rocksdb::WriteBatch::default();
    register_consumer(ctx, &mut batch, &prefix, &stream, &group, &consumer, now);
    let mut frames: Vec<entries::Entry> = Vec::new();
    let mut claimed_ids: Vec<EntryId> = Vec::new();
    // FORCE can MINT a PEL row for an id that was never delivered: the
    // backlog counter only grows for those (rewrites are count-neutral).
    let mut force_created: u64 = 0;
    // PEL rows resolved by dead-letter transfers (not in the reply) and
    // the batch's post-commit accounting (dlq::account_tally).
    let mut dlq_count: u64 = 0;
    let mut tally_sum = dlq::TransferTally::default();
    for id in ids {
        let old = match pel::get_pend(&ctx.shared.store, &prefix, &stream, &group, id) {
            // Not idle enough yet: stays with its current owner.
            Ok(Some(st)) if now.saturating_sub(st.delivered_ms) < min_idle => continue,
            Ok(st) => st,
            Err(e) => {
                offset::restore(&ctx.shared.lite.offsets, &stream, &group, mark.clone());
                return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}"));
            }
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
                Err(e) => {
                    offset::restore(&ctx.shared.lite.offsets, &stream, &group, mark.clone());
                    return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}"));
                }
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
        // The delivery that would pass MAXDELIVERY never happens: the
        // row dead-letters instead (the ONLY claim result that omits an
        // otherwise-legal id). JUSTID never triggers -- an ownership
        // move is not a delivery. Ordered groups only ever claim the
        // head, and a head transfer frees the queue, so no special case.
        let old_times = old.as_ref().map_or(0, |st| st.times_delivered);
        if let Some(g) = gst.as_ref() {
            if !justid && dlq::should_dead_letter(old_times, g.maxdelivery) {
                let row = dlq::DeadRow {
                    id,
                    times: old_times,
                    consumer: consumer.to_vec(),
                };
                match dlq::transfer_entries(
                    dlq::transfer_ctx(ctx.shared, &mut batch, &prefix, &stream, &group, g),
                    std::slice::from_ref(&row),
                    now,
                ) {
                    Ok(t) => {
                        dlq_count += t.resolved as u64;
                        tally_sum.dlq_created += t.dlq_created;
                        tally_sum.moved += t.moved;
                    }
                    Err(e) => {
                        offset::restore(&ctx.shared.lite.offsets, &stream, &group, mark.clone());
                        return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}"));
                    }
                }
                continue;
            }
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
                epoch,
                &hints,
            )),
        );
        match fields {
            Some(fields) if !justid => frames.push(entries::Entry { id, fields }),
            _ => claimed_ids.push(id),
        }
    }
    if let Err(e) = ctx.commit(batch).await {
        // Void batch: rewind the watermark moves the transfers made, so
        // the dead-lettered rows stay pending and claimable.
        offset::restore(&ctx.shared.lite.offsets, &stream, &group, mark);
        return resp::append_error(ctx.out, &format!("ERR: xclaim failed: {e}"));
    }
    dlq::account_tally(ctx.shared, tally_sum);
    if ordered && !(frames.is_empty() && claimed_ids.is_empty()) {
        // Takeover happened: wake blocked readers so a fenced-out former
        // owner (and any contender) re-checks immediately.
        crate::ds::wait::notify(&ctx.shared.wait_hub, &model::meta_key(&prefix, &stream));
    }
    if force_created > 0 {
        offset::bump_pending(
            &ctx.shared.lite.offsets,
            &stream,
            &group,
            force_created as i64,
        );
    }
    if dlq_count > 0 {
        offset::bump_pending(
            &ctx.shared.lite.offsets,
            &stream,
            &group,
            -(dlq_count as i64),
        );
        monitor::observe_lite_message(&ctx.shared.monitor, "dlq", dlq_count);
        if let Some(g) = &gst {
            dlq::notify_transfer(ctx.shared, &prefix, &stream, &g.dlq);
        }
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
        let no_hints = ClaimHints::default();
        let st = claimed_state(4, false, false, b"c2", 1234, 0, &no_hints);
        assert_eq!(st.consumer, b"c2".to_vec());
        assert_eq!(st.delivered_ms, 1234);
        assert_eq!(st.times_delivered, 5); // plain claim: 4 -> 5
                                           // JUSTID moves ownership only: count and delivery stay untouched.
        let st = claimed_state(4, false, true, b"c2", 1234, 0, &no_hints);
        assert_eq!(st.times_delivered, 4);
        assert_eq!(st.delivered_ms, 1234);
        // FORCE-created rows have no prior count: always 1, JUSTID or not.
        assert_eq!(
            claimed_state(0, true, false, b"c2", 1, 0, &no_hints).times_delivered,
            1
        );
        assert_eq!(
            claimed_state(0, true, true, b"c2", 1, 0, &no_hints).times_delivered,
            1
        );
    }

    #[test]
    fn claim_hints_rewrite_clock_and_counter() {
        // IDLE backdates: delivered = now - idle (clamped at 0).
        let st = claimed_state(
            4,
            false,
            false,
            b"c2",
            10_000,
            0,
            &ClaimHints {
                delivery: Some(DeliveryAt::Idle(6_000)),
                retrycount: None,
            },
        );
        assert_eq!(st.delivered_ms, 4_000);
        assert_eq!(st.times_delivered, 5); // no RETRYCOUNT: still bumps
        assert_eq!(
            claimed_state(
                4,
                false,
                false,
                b"c2",
                100,
                0,
                &ClaimHints {
                    delivery: Some(DeliveryAt::Idle(u64::MAX)),
                    retrycount: None,
                }
            )
            .delivered_ms,
            0,
            "IDLE beyond now clamps to epoch 0"
        );
        // TIME parks the wall clock verbatim; idle derives from now.
        let st = claimed_state(
            4,
            false,
            false,
            b"c2",
            10_000,
            0,
            &ClaimHints {
                delivery: Some(DeliveryAt::Time(1_000)),
                retrycount: None,
            },
        );
        assert_eq!(st.delivered_ms, 1_000);
        // RETRYCOUNT replaces the counter, JUSTID or not, fresh or not.
        for (fresh, justid) in [(false, false), (false, true), (true, false)] {
            assert_eq!(
                claimed_state(
                    4,
                    fresh,
                    justid,
                    b"c2",
                    10_000,
                    0,
                    &ClaimHints {
                        delivery: None,
                        retrycount: Some(42),
                    }
                )
                .times_delivered,
                42
            );
        }
    }
}
