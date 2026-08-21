//! Blocking-read parking for XREAD/XREADGROUP: the multi-stream wait
//! loop shared by both read commands. ONE waiter is registered under
//! every DISTINCT target key before the final read, closing the
//! lost-notify window against an XADD committing between the caller's
//! own scan and the registration.

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
/// XADD and group ops alike) and what counts as "data landed" (an
/// entry past `after`).
pub(crate) struct ParkTarget {
    key: Vec<u8>,
    prefix: Vec<u8>,
    stream: Vec<u8>,
    after: EntryId,
    count: usize,
}

/// A spec turned into a wake probe parked at `after`.
pub(crate) fn park_target(s: &StreamSpec, after: EntryId, count: usize) -> ParkTarget {
    ParkTarget {
        key: model::meta_key(&s.prefix, &s.stream),
        prefix: s.prefix.clone(),
        stream: s.stream.clone(),
        after,
        count,
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
/// A SIGNALLED park that still finds no entries returns
/// `Some(Ok(vec![]))` -- "woke, nothing new": the signal may come from
/// a group op (DESTROY / SETID also notify the meta key), and the
/// XREADGROUP caller must re-validate (NOGROUP, rewound watermarks)
/// rather than sleep out its BLOCK. Plain XREAD re-parks.
pub(crate) async fn wait_targets(
    ctx: &mut Ctx<'_>,
    targets: &[ParkTarget],
    block_ms: u64,
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
    loop {
        let waiter = Arc::new(wait::new_waiter());
        for k in &keys {
            wait::register_shared(&ctx.shared.wait_hub, k, &waiter);
        }
        let mut got = Vec::new();
        for t in targets {
            match entries::scan_entries(&ctx.shared.store, &t.prefix, &t.stream, t.after, t.count) {
                Ok(v) if !v.is_empty() => got.push((t.stream.clone(), v)),
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
        // Renewable slice: the remaining budget capped at MAX_SLICE_MS.
        let now = Instant::now();
        let slice = match end {
            None => Duration::from_millis(MAX_SLICE_MS),
            Some(t) if now >= t => {
                unregister_all(ctx, &keys, &waiter);
                return None;
            }
            Some(t) => (t - now).min(Duration::from_millis(MAX_SLICE_MS)),
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
