//! Blocking-read parking for XREAD/XREADGROUP: the multi-stream wait
//! loop shared by both read commands. ONE waiter is registered under
//! every DISTINCT target key before the final read, closing the
//! lost-notify window against an XADD committing between the caller's
//! own scan and the registration.
//!
//! Ordered-group targets whose delivery gate is CLOSED (in-flight
//! window full, or the reader fenced out of the queue) park GATED:
//! their undelivered backlog past `after` IS the backpressure, so it
//! never counts as "data landed" -- a bare scan would hit every time
//! and spin the reader through its whole BLOCK budget. Instead the
//! gate is re-probed after every registration (the same
//! register-before-decide shape, applied to gate-opening events) and
//! park slices are capped at the ownership lease, because lease expiry
//! is passive -- no signal ever fires for it -- and a fenced reader
//! must retry its takeover at lease granularity.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::command::Ctx;
use crate::ds::wait::{self, WaitOutcome, Waiter};
use crate::park;

use super::entries;
use super::model::{self, EntryId};
use super::read::{StreamEntries, StreamSpec};

/// BLOCK 0 means "forever": park in 24h slices so Condvar math stays sane.
const MAX_SLICE_MS: u64 = 86_400_000;

/// One wake probe: where to park (the stream's meta key, notified by
/// XADD, XACK and group ops alike) and what counts as "data landed" (an
/// entry past `after`). A `gated` target (ordered group, delivery gate
/// closed at build time) is EXCLUDED from the data scan and instead
/// watched by the `gate_open` probe -- see the module doc.
pub(crate) struct ParkTarget {
    spec: StreamSpec,
    key: Vec<u8>,
    after: EntryId,
    count: usize,
    gated: bool,
}

/// A spec turned into a wake probe parked at `after`; `gated` marks an
/// ordered-group stream that cannot deliver right now.
pub(crate) fn park_target(s: &StreamSpec, after: EntryId, count: usize, gated: bool) -> ParkTarget {
    ParkTarget {
        key: model::meta_key(&s.prefix, &s.stream),
        spec: s.clone(),
        after,
        count,
        gated,
    }
}

fn unregister_all(ctx: &Ctx<'_>, keys: &[Vec<u8>], waiter: &Arc<Waiter>) {
    for k in keys {
        wait::unregister(&ctx.shared.wait_hub, k, waiter);
    }
}

/// Park until any target stream has entries past its `after` or
/// `block_ms` elapses (`0` = forever, Redis BLOCK 0). Waiting is
/// chunked: every park is at most MAX_SLICE_MS long and the budget is
/// recomputed on each wake, so "forever" loops in renewable slices and
/// oversized BLOCK values are never clamped. ONE waiter is registered
/// under every DISTINCT target key BEFORE the final read -- that closes
/// the lost-notify window against an XADD committing between the
/// caller's own scan and the registration.
///
/// Gated targets (ordered groups whose delivery window is full or
/// whose reader is fenced out) never contribute "data landed": their
/// backlog past `after` is exactly the backpressure being waited out.
/// The same register-before-decide trick covers their gate-OPENING
/// events too: right after registration `gate_open` re-probes every
/// gated target, so an ack or takeover that signalled a key with no
/// waiter yet still returns immediately as "woke, re-validate". When
/// any target is gated, park slices are additionally capped at the
/// ownership lease: lease expiry is passive (nothing notifies it), so
/// a fenced reader retries its takeover at lease granularity instead
/// of sleeping out its whole BLOCK budget.
///
/// A SIGNALLED park that still finds no entries returns
/// `Some(Ok(vec![]))` -- "woke, nothing new": the signal may come from
/// a group op or an XACK that freed the ordered delivery window
/// (DESTROY / SETID also notify the meta key), and the XREADGROUP
/// caller must re-validate (NOGROUP, rewound watermarks, a freed
/// window) rather than sleep out its BLOCK. Plain XREAD re-parks.
pub(crate) async fn wait_targets(
    ctx: &mut Ctx<'_>,
    targets: &[ParkTarget],
    block_ms: u64,
    gate_open: &(dyn Fn(&StreamSpec) -> bool + Sync),
) -> Option<Result<Vec<StreamEntries>, String>> {
    // Distinct meta keys only: a repeated stream name must not
    // register its waiter twice.
    let mut keys: Vec<Vec<u8>> = targets.iter().map(|t| t.key.clone()).collect();
    keys.sort();
    keys.dedup();
    // None = no time limit (BLOCK 0 or oversized); Some(t) = expiry.
    let end = if block_ms == 0 {
        None
    } else {
        Instant::now().checked_add(Duration::from_millis(block_ms))
    };
    // Gated presence is fixed for this round (the caller recomputes
    // gated flags at every loop-head pass); the lease cap rides along
    // so a fenced reader's slices stay at takeover-retry granularity.
    let gated_any = targets.iter().any(|t| t.gated);
    let slice_cap_ms = if gated_any {
        // max(1): a zero lease must degrade to the finest retry, not a
        // zero-length park that would spin.
        MAX_SLICE_MS.min(ctx.shared.lite.lease_ms().max(1))
    } else {
        MAX_SLICE_MS
    };
    loop {
        let waiter = Arc::new(wait::new_waiter());
        for k in &keys {
            wait::register_shared(&ctx.shared.wait_hub, k, &waiter);
        }
        // Register -> gate re-check: a gate-opening event (ack freed
        // the window, a takeover deposed the owner, the group vanished)
        // that ran between the caller's loop head and the registration
        // already notified a key with NO waiter -- ONE waiter is
        // registered BEFORE this probe, closing that lost-notify race
        // exactly like the data scan below. Cheap by contract: cache
        // and owner-map reads only (see the caller's predicate).
        if targets.iter().any(|t| t.gated && gate_open(&t.spec)) {
            unregister_all(ctx, &keys, &waiter);
            return Some(Ok(Vec::new()));
        }
        let mut got = Vec::new();
        for t in targets {
            // Gated targets skip the data probe entirely: backlog past
            // `after` is EXPECTED here and would return every round.
            if t.gated {
                continue;
            }
            match entries::scan_entries(
                &ctx.shared.store,
                &t.spec.prefix,
                &t.spec.stream,
                t.after,
                t.count,
            ) {
                Ok(v) if !v.is_empty() => got.push((t.spec.stream.clone(), v)),
                Ok(_) => {}
                // One unreadable stream fails the whole command.
                Err(e) => {
                    unregister_all(ctx, &keys, &waiter);
                    return Some(Err(e));
                }
            }
        }
        if !got.is_empty() {
            unregister_all(ctx, &keys, &waiter);
            return Some(Ok(got));
        }
        // Renewable slice: the remaining budget capped at MAX_SLICE_MS
        // (and at the ownership lease when gated targets are parked).
        let now = Instant::now();
        let slice = match end {
            None => Duration::from_millis(slice_cap_ms),
            Some(t) if now >= t => {
                unregister_all(ctx, &keys, &waiter);
                return None;
            }
            Some(t) => (t - now).min(Duration::from_millis(slice_cap_ms)),
        };
        let w = Arc::clone(&waiter);
        let woke = park::park(move || wait::wait(&w, slice)).await;
        unregister_all(ctx, &keys, &waiter);
        match woke.unwrap_or(WaitOutcome::Timeout) {
            // Budget spent: nil (the caller maps None to the nil reply).
            WaitOutcome::Timeout if end.is_some_and(|t| Instant::now() >= t) => return None,
            // Signalled, budget left, no entries: hand the wake back
            // for caller-side re-validation (see above).
            WaitOutcome::Signaled => return Some(Ok(Vec::new())),
            // Spurious wake or a slice edge with budget left: re-read.
            WaitOutcome::Timeout => {}
        }
    }
}

/// Remaining BLOCK budget handed to one `wait_targets` round in a
/// blocking loop. `None` means the deadline already passed -- the
/// caller replies nil now instead of parking. `Some(ms)` is clamped to
/// at least 1: a sub-millisecond remainder truncated to 0 would be read
/// by `wait_targets` as BLOCK 0 ("wait forever"), hanging a bounded
/// read past its deadline. (`None` as `end` is the forever sentinel and
/// passes the raw `block_ms` through.)
pub(crate) fn remaining_ms(end: Option<Instant>, block_ms: u64) -> Option<u64> {
    let t = match end {
        // No deadline (BLOCK 0 / oversized): the caller's forever
        // sentinel passes through untouched.
        None => return Some(block_ms),
        Some(t) => t,
    };
    let now = Instant::now();
    if now >= t {
        return None;
    }
    Some(((t - now).as_millis() as u64).max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_ms_passes_forever_sentinel_and_rejects_expired() {
        // No deadline (BLOCK 0 / oversized): the raw block_ms sentinel
        // goes through untouched.
        assert_eq!(remaining_ms(None, 7000), Some(7000));
        // An already-passed deadline: park nothing, reply nil now.
        assert_eq!(
            remaining_ms(Some(Instant::now() - Duration::from_millis(1)), 5),
            None
        );
        // A live deadline: (nearly) all of the remaining milliseconds
        // (a tick may elapse between building the deadline and the
        // measurement, so assert a small window, not exact equality).
        let got = remaining_ms(Some(Instant::now() + Duration::from_millis(10)), 5);
        assert!(
            matches!(got, Some(ms) if (8..=10).contains(&ms)),
            "expected ~10ms, got {got:?}"
        );
    }

    #[test]
    fn remaining_ms_clamps_sub_millisecond_remainder_to_one() {
        // Regression: 0 < remaining < 1ms truncated to 0, which
        // wait_targets reads as BLOCK 0 = wait FOREVER; a bounded
        // XREADGROUP could hang past its deadline until an append woke
        // it. The remainder must clamp up to 1ms so the timeout fires.
        let sub = Instant::now() + Duration::from_micros(400);
        let got = remaining_ms(Some(sub), 5);
        assert_eq!(got, Some(1), "sub-millisecond budget must be 1ms, not 0");
    }
}
