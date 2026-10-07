//! Idle consumer GC (`lite.consumer_gc_ms`): the background reclaimer
//! of dead group members -- consumer registry rows whose owners went
//! away. A consumer is collectable only when ALL THREE hold (the
//! triple criteria; any miss keeps it):
//!
//! 1. **no PEL entries** -- zero pending rows name it (the round walks
//!    the group's whole PEL once and proves emptiness; a group whose
//!    PEL is deeper than [`PEL_PROOF_BUDGET`] is skipped a round rather
//!    than guessed at);
//! 2. **no active lease** -- it is neither parked in a waiting
//!    XREADGROUP ([`Runtime::park_acquire`], refcounted around the
//!    await in `read::xreadgroup`) nor the live owner of an ordered
//!    queue (`ordered::peek` + `lease_live`: the ownership lease the
//!    ordered machinery already maintains covers its owners);
//! 3. **idle past the threshold** -- `now - seen_ms >= consumer_gc_ms`
//!    where `seen_ms` is the consumer-registry activity clock stamped
//!    by every write that already syncs (delivery, XCLAIM/XAUTOCLAIM,
//!    XACK of owned rows; legacy rows fall back to `created_ms`).
//!
//! Removal reuses the XGROUP DELCONSUMER path verbatim
//! ([`group::plan_consumer_removals`] + [`group::consumer_removal_effects`]),
//! committed through one synced `ops::batch_write` under the stream's
//! meta latch (opportunistic `try_lock`, the redeliver convention: a
//! mid-command stream is skipped one round, never parked on) -- so
//! group counters, ordered ownership and the runtime registry stay
//! consistent, and a collected member cannot reappear after kill -9.
//! The GC keeps NO state of its own beyond a rotating group-discovery
//! cursor (the redeliver sweep's resume map is untouched: rounds of the
//! two jobs interleave without disturbing each other's cursors).

use std::sync::Arc;
use std::time::Duration;

use crate::ds::{expire, latch};
use crate::state;
use crate::store::ops;

use super::redeliver::Discovered;
use super::{group, model, ordered, pel, redeliver};

/// GC rhythm, ms (slower than the flusher's 200ms: collection latency
/// is unobserved, the threshold is minutes in practice, and each round
/// walks a full PEL per group).
pub(crate) const PERIOD_MS: u64 = 1000;
/// PEL rows walked per group per round: emptiness must be PROVEN, so a
/// deeper PEL skips the group this round (rotation still reaches every
/// other group; the skipped one shrinks or splits as it drains).
pub(crate) const PEL_PROOF_BUDGET: usize = 1024;

/// Verdict for one registry consumer (pure: facts in, one fate out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// An active or recent member: keeps its registry row.
    Keep,
    /// Empty PEL, no lease, idle past the threshold: collectable.
    Collect,
}

/// The triple criteria as one pure decision. `seen_ms` is the consumer's
/// last activity stamp (legacy rows: `created_ms`).
pub(crate) fn verdict(
    now_ms: u64,
    threshold_ms: u64,
    seen_ms: u64,
    has_pel: bool,
    leased: bool,
) -> Verdict {
    if has_pel || leased {
        return Verdict::Keep;
    }
    if now_ms.saturating_sub(seen_ms) < threshold_ms {
        return Verdict::Keep;
    }
    Verdict::Collect
}

/// The activity stamp of a scanned registry row: rows written before
/// `seen_ms` existed decode it as 0 and conservatively fall back to the
/// creation time (the same approximation XINFO uses for seen-time).
pub(crate) fn seen_of(created_ms: u64, seen_ms: u64) -> u64 {
    seen_ms.max(created_ms)
}

/// Active-lease veto #2: parked in a waiting XREADGROUP, or holding a
/// LIVE ordered-group ownership lease (a stale lease is abandonware --
/// the queue migrates on the next ask, the row may go).
fn leased(shared: &state::Shared, stream: &[u8], group: &[u8], consumer: &[u8], now: u64) -> bool {
    if shared.lite.is_parked(stream, group, consumer) {
        return true;
    }
    match ordered::peek(&shared.lite.owners, stream, group) {
        Some(o) => {
            o.consumer == consumer && ordered::lease_live(o.active_ms, now, shared.lite.lease_ms())
        }
        None => false,
    }
}

/// One full GC round from the head (tests, cold starts); the spawn loop
/// uses [`gc_from`] to rotate its discovery cursor. Returns the number
/// of collected consumers.
pub fn gc_once(shared: &state::Shared, now_ms: u64) -> usize {
    gc_from(shared, now_ms, &[]).0
}

/// One GC round resuming strictly after `from` (the previous round's
/// group-discovery cursor, the redeliver pattern): (collected, next
/// cursor; empty restarts the next round at the head).
pub fn gc_from(shared: &state::Shared, now_ms: u64, from: &[u8]) -> (usize, Vec<u8>) {
    let threshold = shared.conf.lite.consumer_gc_ms;
    if threshold == 0 {
        return (0, from.to_vec()); // disabled: never scheduled anyway
    }
    let Ok((groups, cursor)) = redeliver::discover_groups(&shared.store, from) else {
        return (0, from.to_vec());
    };
    let collected = groups
        .iter()
        .map(|g| gc_group(shared, threshold, now_ms, g))
        .sum();
    (collected, cursor)
}

/// GC one group: opportunistic meta latch (a mid-command stream is
/// skipped one round), one full-PEL emptiness proof, then ONE synced
/// batch of registry-row deletions through the shared DELCONSUMER plan.
fn gc_group(shared: &state::Shared, threshold: u64, now: u64, g: &Discovered) -> usize {
    let Some(prefix) = model::stream_prefix(&g.stream) else {
        return 0;
    };
    let Some(_guard) = latch::try_lock(&shared.latch, &model::meta_key(&prefix, &g.stream)) else {
        return 0; // mid-command: next round
    };
    // Emptiness proof: the group's WHOLE PEL in one bounded walk. A
    // skipped scan (store error) or an over-budget PEL proves nothing
    // -- the group waits a round, nothing is guessed at.
    let Ok(rows) = pel::scan_pend(
        &shared.store,
        &prefix,
        &g.stream,
        &g.group,
        model::MIN_ID,
        Some(PEL_PROOF_BUDGET + 1),
    ) else {
        return 0;
    };
    if rows.len() > PEL_PROOF_BUDGET {
        return 0;
    }
    let Ok(registry) = pel::scan_consumers(&shared.store, &prefix, &g.stream, &g.group) else {
        return 0;
    };
    // Owners of record: any consumer named by a live PEL row is kept by
    // criterion 1 without touching its registry stamp.
    let mut doomed: Vec<Vec<u8>> = Vec::new();
    for c in &registry {
        let has_pel = rows.iter().any(|r| r.state.consumer == c.name);
        if verdict(
            now,
            threshold,
            seen_of(c.created_ms, c.seen_ms),
            has_pel,
            leased(shared, &g.stream, &g.group, &c.name, now),
        ) == Verdict::Collect
        {
            doomed.push(c.name.clone());
        }
    }
    if doomed.is_empty() {
        return 0;
    }
    // The shared DELCONSUMER removal plan (row purge + registry delete)
    // in ONE synced batch; doomed names are PEL-empty by construction,
    // so the purge pass finds nothing -- the code path stays identical.
    let names: Vec<&[u8]> = doomed.iter().map(|n| n.as_slice()).collect();
    let (batch, _) = group::plan_consumer_removals(&prefix, &g.stream, &g.group, &names, &rows);
    if ops::batch_write(&shared.store, batch).is_err() {
        return 0; // store failed: registry intact, next round retries
    }
    for name in &doomed {
        group::consumer_removal_effects(shared, &g.stream, &g.group, name, 0);
    }
    doomed.len()
}

/// Background GC task: one [`gc_from`] round every [`PERIOD_MS`] on the
/// blocking pool, discovery cursor rotating between rounds; no task at
/// all unless `lite.consumer_gc_ms` is configured (the 0 default keeps
/// the upgrade path at zero behavior change). Log-spam-free: only store
/// failures surface, through the round's own error handling.
pub fn spawn_consumer_gc(shared: Arc<state::Shared>) {
    if shared.conf.lite.consumer_gc_ms == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(PERIOD_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        let mut cursor: Vec<u8> = Vec::new();
        loop {
            ticker.tick().await;
            // Sync scans + one synced write per collecting group:
            // blocking pool, never a tokio worker; a JoinError keeps
            // the cursor, next tick.
            let (sh, from) = (Arc::clone(&shared), cursor.clone());
            if let Ok(next) =
                tokio::task::spawn_blocking(move || gc_from(&sh, expire::now_ms(), &from).1).await
            {
                cursor = next;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_needs_all_three_conditions() {
        // Idle + empty + unleased: the only collectable shape.
        assert_eq!(
            verdict(10_000, 1_000, 8_000, false, false),
            Verdict::Collect
        );
        // Criterion 1: a PEL holder is never collected, however idle.
        assert_eq!(verdict(10_000, 1_000, 0, true, false), Verdict::Keep);
        // Criterion 2: a leased (parked / ordered-owner) member survives.
        assert_eq!(verdict(10_000, 1_000, 0, false, true), Verdict::Keep);
        // Criterion 3: one ms below the threshold is still recent.
        assert_eq!(verdict(10_000, 1_000, 9_001, false, false), Verdict::Keep);
        assert_eq!(
            verdict(10_000, 1_000, 9_000, false, false),
            Verdict::Collect
        );
        // Clock skew (a future stamp) reads as fresh, never negative-idle.
        assert_eq!(verdict(1_000, 1_000, 9_000, false, false), Verdict::Keep);
    }

    #[test]
    fn legacy_rows_fall_back_to_creation_time() {
        // Pre-seen_ms rows decode seen as 0: the creation time is the
        // conservative floor (never younger than the real last sight,
        // so a legacy member is kept at least threshold-long).
        assert_eq!(seen_of(500, 0), 500);
        assert_eq!(seen_of(500, 900), 900);
    }
}
