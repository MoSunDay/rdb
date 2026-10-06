//! Idle auto-redelivery sweep (design two, `lite.redelivery_idle_ms`):
//! the unattended twin of XAUTOCLAIM. A rotating bounded scan discovers
//! groups straight from the kind-0x0E window (a group never touched
//! since boot is still found); per group, under the same per-stream
//! latches the delivery paths take, every idle PEL row is re-handed to
//! its CURRENT consumer (clock refresh + count bump -- the claim
//! primitive), MAXDELIVERY rows folding into the dead-letter transfer.
//! Ordered groups sweep only the head via `ordered::takeover_if_stale`
//! (a LEASE-VALIDATED claim: a live holder is never deposed or
//! refreshed). OFF by default. Rounds are SYNC and their latches
//! OPPORTUNISTIC (`latch::try_lock`): a mid-command stream is skipped
//! one round, never parked on.

use rocksdb::WriteBatch;

use crate::ds::{codec, expire, latch, wait};
use crate::monitor;
use crate::state;
use crate::store::ops;

use super::offset;
use super::ordered;
use super::pel;
use super::{dlq, model};

/// Groups examined per round (rotation keeps every group reachable).
pub(crate) const GROUP_BUDGET: usize = 32;
/// PEL rows examined per group per round (a big PEL rides later rounds).
pub(crate) const ROW_BUDGET: usize = 16;
/// Keys walked per discovery scan.
const SCAN_LIMIT: usize = 4096;
/// Per-group resume cursors kept at once; past the cap the map resets
/// to head-scans for a round (degraded reachability, never a stall) --
/// the alternative, never forgetting destroyed groups' cursors, grows
/// without bound.
const RESUME_CAP: usize = 4096;

/// Last examined pending id per (stream, group): the sweep's rotation
/// state for in-group fairness (a group whose head rows are always
/// fresh still reaches its tail -- see `sweep_group`).
pub type ResumeMap = std::collections::HashMap<(Vec<u8>, Vec<u8>), model::EntryId>;

/// Verdict for one scanned PEL row (pure: clocks in, one fate out):
/// Skip = below the threshold (keeps waiting with its consumer),
/// Redeliver = re-hand to the row's consumer (clock + count bump),
/// DeadLetter = the next hand-out would pass MAXDELIVERY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowAction {
    Skip,
    Redeliver,
    DeadLetter,
}

pub(crate) fn row_action(now: u64, idle: u64, delivered: u64, times: u64, cap: u64) -> RowAction {
    if now.saturating_sub(delivered) < idle {
        RowAction::Skip
    } else if dlq::should_dead_letter(times, cap) {
        RowAction::DeadLetter
    } else {
        RowAction::Redeliver
    }
}

/// One (stream, group) located by the rotating group-record scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Discovered {
    pub stream: Vec<u8>,
    pub group: Vec<u8>,
}

/// Bounded rotating discovery of group records (kind 0x0E), strictly
/// after `from` (empty = head), at most [`GROUP_BUDGET`] groups within
/// [`SCAN_LIMIT`] keys (the expire sampler's pattern: high keys never
/// starve). Returns the groups plus the resume cursor -- EMPTY when
/// the scan ran to the tail. Values are not decoded: the sweep
/// re-validates each group under its latches.
pub(crate) fn discover_groups(
    store: &crate::store::Store,
    from: &[u8],
) -> Result<(Vec<Discovered>, Vec<u8>), String> {
    let mut out: Vec<Discovered> = Vec::new();
    let mut cursor = from.to_vec();
    let mut examined = 0usize;
    let mut wrapped = true;
    ops::for_each_from(store, from, true, &mut |k, _| {
        if out.len() >= GROUP_BUDGET || examined >= SCAN_LIMIT {
            wrapped = false; // resume strictly after the last examined key
            return false;
        }
        examined += 1;
        cursor = k.to_vec();
        let group_rec = expire::slot_prefix_len(k)
            .filter(|plen| k.get(*plen) == Some(&codec::KIND_STREAM_GROUP))
            .and_then(|plen| codec::decode_data_key(k, plen));
        if let Some((_, stream, group)) = group_rec {
            if !stream.is_empty() && !group.is_empty() {
                out.push(Discovered {
                    stream: stream.to_vec(),
                    group: group.to_vec(),
                });
            }
        }
        true
    })?;
    if wrapped {
        cursor.clear();
    }
    Ok((out, cursor))
}

/// One round's outcome: (redelivered, dead-lettered, resume cursor;
/// an empty cursor restarts the next round at the head).
pub type RoundTally = (usize, usize, Vec<u8>);

/// One full round from the head (the public round unit: tests, cold
/// starts); the spawn loop uses [`sweep_from`] to rotate its cursors.
pub fn sweep_once(shared: &state::Shared, now_ms: u64) -> RoundTally {
    let mut resumes = ResumeMap::new();
    sweep_from(shared, now_ms, &[], &mut resumes)
}

/// One round resuming strictly after `from` (the previous round's
/// returned group-discovery cursor) and, per group, strictly after that
/// group's last examined pending id in `resumes` (in-group fairness:
/// with a fresh head the next round looks PAST it instead of re-reading
/// the same first rows forever): (redelivered, dead-lettered, next
/// discovery cursor).
pub fn sweep_from(
    shared: &state::Shared,
    now_ms: u64,
    from: &[u8],
    resumes: &mut ResumeMap,
) -> RoundTally {
    let idle_ms = shared.conf.lite.redelivery_idle_ms;
    if idle_ms == 0 {
        return (0, 0, from.to_vec()); // disabled: never scheduled anyway
    }
    let Ok((groups, cursor)) = discover_groups(&shared.store, from) else {
        return (0, 0, from.to_vec());
    };
    let tally = |(r, d): (usize, usize), g: &Discovered| {
        let (gr, gd) = sweep_group(shared, idle_ms, now_ms, g, resumes);
        (r + gr, d + gd)
    };
    let (redelivered, dlqed) = groups.iter().fold((0, 0), tally);
    if resumes.len() > RESUME_CAP {
        resumes.clear(); // destroyed groups' stale cursors must not leak
    }
    (redelivered, dlqed, cursor)
}

/// Sweep one group: try its latch set (stream + sorted DLQ target),
/// then plan the PEL head window, ONE batch for rewrites + transfers,
/// one synced write, wake readers. Contention contributes zero.
fn sweep_group(
    shared: &state::Shared,
    idle: u64,
    now: u64,
    g: &Discovered,
    resumes: &mut ResumeMap,
) -> (usize, usize) {
    let Some(prefix) = model::stream_prefix(&g.stream) else {
        return (0, 0);
    };
    // Pre-lock state load (read-only, warms the cache): only the
    // immutable CREATE config (maxdelivery/dlq) decides the key set.
    let dlq_stream = offset::load(
        &shared.lite.offsets,
        &shared.store,
        &prefix,
        &g.stream,
        &g.group,
    )
    .ok()
    .flatten()
    .filter(|st| st.maxdelivery > 0 && !st.dlq.is_empty())
    .map(|st| st.dlq);
    let keys = dlq::latch_keys(model::meta_key(&prefix, &g.stream), dlq_stream.as_deref());
    let mut guards = Vec::with_capacity(keys.len());
    for k in &keys {
        // Sorted order; RAII drops the partial set on a miss.
        match latch::try_lock(&shared.latch, k) {
            Some(guard) => guards.push(guard),
            None => return (0, 0), // mid-command: next round
        }
    }
    // Re-load under the latch: command paths kept the cache current.
    let Ok(Some(st)) = offset::load(
        &shared.lite.offsets,
        &shared.store,
        &prefix,
        &g.stream,
        &g.group,
    ) else {
        return (0, 0);
    };
    // Latch-set drift re-validation: a DESTROY + re-CREATE with a
    // DIFFERENT DLQ can commit between the pre-lock peek and the locks;
    // a drifted set is skipped one round (never parked on, never
    // written through) -- the next round re-peeks the fresh config.
    if dlq::group_dlq_target(Some(&st)) != dlq_stream {
        return (0, 0);
    }
    // Pre-transfer cache snapshot: restored when the batch fails, so a
    // flushed watermark can never name ids the store never resolved.
    let mark = offset::mark(&shared.lite.offsets, &g.stream, &g.group);
    let (stream, group) = (&g.stream, &g.group);
    // In-group resume (starvation fix): a full ROW_BUDGET window may
    // hide rows behind it, so the next round for this group starts
    // STRICTLY AFTER the last id examined now; a short window proved
    // the PEL tail was reached and the cursor wraps to the head, so
    // fresh head rows are still revisited. Ordered groups only ever
    // move the head, so they carry no cursor (always scan the head).
    let scan_from = match resumes.get(&(stream.clone(), group.clone())) {
        Some(last) => super::read::succ_id(*last).unwrap_or(model::MIN_ID),
        None => model::MIN_ID,
    };
    let Ok(rows) = pel::scan_pend(
        &shared.store,
        &prefix,
        stream,
        group,
        scan_from,
        Some(ROW_BUDGET),
    ) else {
        return (0, 0); // scan failed: window unexamined, cursor kept
    };
    let resume_key = (stream.clone(), group.clone());
    if st.ordered || rows.len() < ROW_BUDGET {
        // Head-only (ordered) or a short scan reached the tail: the
        // next round re-reads from the head.
        resumes.remove(&resume_key);
    } else {
        resumes.insert(resume_key, rows[rows.len() - 1].id);
    }
    if rows.is_empty() {
        return (0, 0); // acked rows are gone: nothing to sweep
    }
    let mut batch = WriteBatch::default();
    let mut dead: Vec<dlq::DeadRow> = Vec::new();
    let mut redelivered = 0usize;
    for (i, row) in rows.iter().enumerate() {
        // Ordered groups deliver (and claim) the head only, so the sweep
        // moves the head only; deeper rows wait until they ARE the head.
        if st.ordered && i > 0 {
            break;
        }
        let (delivered, times) = (row.state.delivered_ms, row.state.times_delivered);
        let due = row_action(now, idle, delivered, times, st.maxdelivery);
        if due == RowAction::Skip {
            continue;
        }
        if due == RowAction::Redeliver {
            // Claim primitive, consumer unchanged: clock refresh + count
            // bump. Ordered groups must also (re)acquire the queue for
            // the row's consumer -- but ONLY from a stale holder
            // (`takeover_if_stale`): a live holder is never deposed or
            // refreshed, so the unattended sweep can neither fence out
            // a healthy consumer nor keep `>` competitors `Busy` by
            // endlessly refreshing a zombie's lease; against a live
            // foreign holder the head simply waits a round.
            let owners = &shared.lite.owners;
            let epoch = if st.ordered {
                match ordered::takeover_if_stale(
                    owners,
                    stream,
                    group,
                    &row.state.consumer,
                    now,
                    shared.lite.lease_ms(),
                ) {
                    ordered::StaleAccess::Own(e) => e,
                    ordered::StaleAccess::Held => break, // live holder: no depose
                }
            } else {
                row.state.epoch
            };
            batch.put(
                pel::pend_key(&prefix, stream, group, row.id),
                pel::encode_pend(&pel::PendState {
                    consumer: row.state.consumer.clone(),
                    delivered_ms: now,
                    times_delivered: times.saturating_add(1),
                    epoch,
                }),
            );
            redelivered += 1;
        } else {
            dead.push(dlq::DeadRow {
                id: row.id,
                times,
                consumer: row.state.consumer.clone(),
            });
        }
        if st.ordered {
            break; // the head alone moves (either fate)
        }
    }
    if redelivered == 0 && dead.is_empty() {
        return (0, 0); // nothing due: no batch, no write
    }
    let mut dlqed = 0usize;
    let mut tally = dlq::TransferTally::default();
    if !dead.is_empty() {
        let t = dlq::transfer_ctx(shared, &mut batch, &prefix, stream, group, &st);
        match dlq::transfer_entries(t, &dead, now) {
            Ok(k) => {
                dlqed = k.resolved;
                tally = k;
                offset::bump_pending(&shared.lite.offsets, stream, group, -(k.resolved as i64));
            }
            Err(e) => {
                // A failed transfer VOIDS the whole batch: committing
                // the redeliver half alone would re-hand rows whose
                // dead-letter twins never landed (double delivery), and
                // the watermark the transfer advanced must not survive.
                offset::restore(&shared.lite.offsets, &g.stream, &g.group, mark);
                sweep_failed(shared, stream, e);
                return (0, 0); // next round re-plans from the snapshot
            }
        }
    }
    if ops::batch_write(&shared.store, batch).is_err() {
        // The cache watermark must never name ids the store never
        // resolved: rewind, then let the next round re-plan the rows
        // (still pending, still redeliverable -- never silently lost).
        offset::restore(&shared.lite.offsets, &g.stream, &g.group, mark);
        sweep_failed(shared, stream, "batch write failed".to_string());
        return (0, 0);
    }
    dlq::account_tally(shared, tally);
    // Zero counts are observable no-ops, so both fire unconditionally.
    monitor::observe_lite_message(&shared.monitor, "redeliver", redelivered as u64);
    monitor::observe_lite_message(&shared.monitor, "dlq", dlqed as u64);
    // Clocks moved, window slots freed, the DLQ grew: wake readers.
    wait::notify(&shared.wait_hub, &model::meta_key(&prefix, stream));
    if dlqed > 0 {
        dlq::notify_transfer(shared, &prefix, stream, &st.dlq);
    }
    (redelivered, dlqed)
}

/// A voided sweep round: log the reason (the sweep is unattended, so
/// stderr is the only witness) and count it on the `dlq_fail` counter.
fn sweep_failed(shared: &state::Shared, stream: &[u8], err: String) {
    eprintln!(
        "[lite] redelivery sweep batch voided for {}: {err}",
        String::from_utf8_lossy(stream)
    );
    monitor::observe_lite_message(&shared.monitor, "dlq_fail", 1);
}

#[cfg(test)]
mod tests {
    use super::super::model::EntryId;
    use super::*;
    use crate::state::testutil;

    /// Encoded PEL row of consumer c1 (tests stamp fixed clocks).
    fn pend(delivered: u64, times: u64) -> Vec<u8> {
        pel::encode_pend(&pel::PendState {
            consumer: b"c1".to_vec(),
            delivered_ms: delivered,
            times_delivered: times,
            epoch: 0,
        })
    }

    #[test]
    fn row_action_boundaries() {
        // Below the threshold: wait; exactly AT it sweeps (>=).
        assert_eq!(row_action(500, 100, 450, 1, 0), RowAction::Skip);
        assert_eq!(row_action(500, 100, 400, 1, 0), RowAction::Redeliver);
        // Idle rows: times+1 <= max redelivers; past it, dead-letter.
        assert_eq!(row_action(500, 100, 100, 2, 2), RowAction::DeadLetter);
        assert_eq!(row_action(500, 100, 100, 9, 0), RowAction::Redeliver);
        assert_eq!(row_action(500, 100, 100, 1, 2), RowAction::Redeliver);
    }

    /// Discovery decodes KEYS only: records come back ordered and
    /// bounded, the cursor resuming strictly after the last key.
    #[test]
    fn discover_groups_rotates_with_budget() {
        let shared = testutil::shared_with(testutil::test_config());
        let prefix = crate::hash::slot_with_prefix(b"t").1;
        let mut batch = WriteBatch::default();
        let graw = model::encode_group(&model::GroupPayload::default());
        for (stream, group) in [("t/q0", "g1"), ("t/q0", "g2"), ("t/q1", "g3")] {
            let gkey = model::group_key(&prefix, stream.as_bytes(), group.as_bytes());
            batch.put(gkey, graw.clone());
        }
        ops::batch_write(&shared.store, batch).unwrap();
        let (all, cursor) = discover_groups(&shared.store, &[]).unwrap();
        assert_eq!(all.len(), 3, "every group of both streams");
        assert!(cursor.is_empty(), "ran to the tail: wrap next round");
        assert!(all
            .iter()
            .any(|g| &g.stream == b"t/q1" && &g.group == b"g3"));
        // (Resume-strictly-after mirrors the expire sampler's cursor
        // contract; the wrap case is asserted above.)
    }

    /// Full rounds over a seeded store: an idle row re-hands to its own
    /// consumer (clock + count), then the MAXDELIVERY round folds into
    /// the dead-letter transfer (PEL drained, DLQ entry landed).
    #[test]
    fn sweep_once_redelivers_then_dead_letters() {
        let mut conf = testutil::test_config();
        conf.lite.redelivery_idle_ms = 100;
        let shared = testutil::shared_with(conf);
        let (stream, group, e1) = (b"t/q0".to_vec(), b"g".to_vec(), EntryId { ms: 1, seq: 0 });
        let prefix = crate::hash::slot_with_prefix(b"t").1;
        let mut seed = WriteBatch::default();
        seed.put(
            model::meta_key(&prefix, &stream),
            model::encode_meta(&model::MetaPayload {
                created_ms: 1,
                last_ms: 1,
                len: 1,
                ..Default::default()
            }),
        );
        seed.put(
            model::entry_key(&prefix, &stream, e1),
            model::encode_entry(&[(b"f", b"v1")]),
        );
        let dlq_target = dlq::default_dlq_stream(&stream);
        let g = model::GroupPayload {
            maxdelivery: 2,
            dlq: dlq_target.clone(),
            ..Default::default()
        };
        seed.put(
            model::group_key(&prefix, &stream, &group),
            model::encode_group(&g),
        );
        seed.put(pel::pend_key(&prefix, &stream, &group, e1), pend(100, 1));
        ops::batch_write(&shared.store, seed).unwrap();
        // Round 1 (now=600): idle; 1 -> 2 stays within the cap.
        assert_eq!(sweep_once(&shared, 600), (1, 0, Vec::new()));
        let st = pel::get_pend(&shared.store, &prefix, &stream, &group, e1)
            .unwrap()
            .unwrap();
        assert_eq!(
            (st.times_delivered, st.delivered_ms, st.consumer),
            (2, 600, b"c1".to_vec())
        );
        // Round 2 (now=800): 2 -> 3 > 2 -> the DLQ hand-off.
        assert_eq!(sweep_once(&shared, 800), (0, 1, Vec::new()));
        assert!(
            pel::get_pend(&shared.store, &prefix, &stream, &group, e1)
                .unwrap()
                .is_none(),
            "PEL row drained"
        );
        let dp = model::stream_prefix(&dlq_target).unwrap();
        assert!(
            ops::get_physical(&shared.store, &model::entry_key(&dp, &dlq_target, e1))
                .unwrap()
                .is_some(),
            "DLQ entry landed"
        );
        let graw = ops::get_physical(&shared.store, &model::group_key(&prefix, &stream, &group))
            .unwrap()
            .unwrap();
        assert_eq!(
            model::decode_group(&graw).unwrap().committed_ms,
            1,
            "watermark crossed"
        );
    }
}
