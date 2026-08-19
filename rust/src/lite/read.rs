//! Consuming side: XREAD / XREADGROUP (+ XLEN). Blocking reads park on
//! the shared WaitHub via the dedicated park pool (the hub is a sync
//! Condvar, run off tokio's shared blocking threads);
//! XADD notifies both the stream's and its parent topic's meta keys
//! after its batch commits. XACK lives in [`ack`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::command::Ctx;
use crate::ds::wait::{self, WaitOutcome};
use crate::park;
use crate::resp::codec as resp;

use super::entries::{self, Entry};
use super::model::{self, EntryId, MetaRead};
use super::offset;
use crate::monitor;

/// BLOCK 0 means "forever": park in 24h slices so Condvar math stays sane.
const MAX_SLICE_MS: u64 = 86_400_000;

struct ReadOpts {
    count: usize,
    block_ms: Option<u64>,
}

/// Parse `[COUNT n] [BLOCK ms]` starting at `i`; returns opts + next index.
fn parse_opts(args: &[Vec<u8>], mut i: usize) -> Option<(ReadOpts, usize)> {
    let mut opts = ReadOpts {
        count: 1000,
        block_ms: None,
    };
    while i < args.len() {
        if args[i].eq_ignore_ascii_case(b"COUNT") {
            let n = std::str::from_utf8(args.get(i + 1)?)
                .ok()?
                .parse::<usize>()
                .ok()?;
            if n == 0 {
                return None;
            }
            opts.count = n;
            i += 2;
        } else if args[i].eq_ignore_ascii_case(b"BLOCK") {
            opts.block_ms = Some(
                std::str::from_utf8(args.get(i + 1)?)
                    .ok()?
                    .parse::<u64>()
                    .ok()?,
            );
            i += 2;
        } else {
            break;
        }
    }
    Some((opts, i))
}

fn nil_array(out: &mut Vec<u8>) {
    resp::append_raw(out, b"*-1\r\n");
}

/// Park until entries with id > `after` exist or `block_ms` elapses;
/// `block_ms == 0` means forever (Redis BLOCK 0 semantics). Waiting is
/// chunked: every park is at most MAX_SLICE_MS long and the remaining
/// budget is recomputed on each wake, so "forever" loops in renewable
/// slices and oversized BLOCK values are never clamped to one capped
/// deadline. Registering the waiter BEFORE the final read closes the
/// lost-notify window against XADD.
///
/// A SIGNALLED park that still finds no entries returns
/// `Some(Ok(vec![]))` -- "woke, nothing new" -- instead of re-parking:
/// the signal may come from a group op (XGROUP DESTROY / SETID also
/// notify the stream's meta key), and the XREADGROUP caller must
/// re-validate the group (NOGROUP, rewound watermarks) rather than sleep
/// out its BLOCK. Plain XREAD treats the empty wake as a re-park.
async fn wait_entries(
    ctx: &mut Ctx<'_>,
    prefix: &[u8],
    stream: &[u8],
    after: EntryId,
    count: usize,
    block_ms: u64,
) -> Option<Result<Vec<Entry>, String>> {
    let wake = model::meta_key(prefix, stream);
    // None = no time limit (BLOCK 0, or a value too large for Instant);
    // Some(t) = absolute expiry.
    let end = if block_ms == 0 {
        None
    } else {
        Instant::now().checked_add(Duration::from_millis(block_ms))
    };
    loop {
        let waiter = wait::register(&ctx.shared.wait_hub, &wake);
        match entries::scan_entries(&ctx.shared.store, prefix, stream, after, count) {
            Err(e) => {
                wait::unregister(&ctx.shared.wait_hub, &wake, &waiter);
                return Some(Err(e));
            }
            Ok(v) if !v.is_empty() => {
                wait::unregister(&ctx.shared.wait_hub, &wake, &waiter);
                return Some(Ok(v));
            }
            Ok(_) => {}
        }
        // Renewable slice: whatever remains of the budget, capped at
        // MAX_SLICE_MS; an unbounded wait parks full slices forever.
        let now = Instant::now();
        let slice = match end {
            None => Duration::from_millis(MAX_SLICE_MS),
            Some(t) if now >= t => {
                wait::unregister(&ctx.shared.wait_hub, &wake, &waiter);
                return None;
            }
            Some(t) => (t - now).min(Duration::from_millis(MAX_SLICE_MS)),
        };
        let w = Arc::clone(&waiter);
        let woke = park::park(move || wait::wait(&w, slice)).await;
        wait::unregister(&ctx.shared.wait_hub, &wake, &waiter);
        match woke.unwrap_or(WaitOutcome::Timeout) {
            // Budget spent: nil (the caller maps None to the nil reply).
            WaitOutcome::Timeout if end.is_some_and(|t| Instant::now() >= t) => return None,
            // Signalled with budget left but no entries landed: hand the
            // wake back for caller-side re-validation (see above).
            WaitOutcome::Signaled => return Some(Ok(Vec::new())),
            // Spurious wake or a slice edge with budget left: re-read.
            WaitOutcome::Timeout => {}
        }
    }
}

fn append_read_reply(out: &mut Vec<u8>, stream: &[u8], entries: &[Entry]) {
    resp::append_array(out, 1);
    resp::append_array(out, 2);
    resp::append_bulk(out, stream);
    resp::append_array(out, entries.len());
    for e in entries {
        entries::append_entry_frame(out, e);
    }
}

/// Remaining BLOCK budget handed to one `wait_entries` round in
/// XREADGROUP's loop. `None` means the deadline already passed — the
/// caller replies nil now instead of parking. `Some(ms)` is clamped to
/// at least 1: a sub-millisecond remainder truncated to 0 would be read
/// by `wait_entries` as BLOCK 0 ("wait forever"), hanging a bounded
/// read past its deadline. (`None` as `end` is the forever sentinel and
/// passes the raw `block_ms` through.)
fn remaining_ms(end: Option<Instant>, block_ms: u64) -> Option<u64> {
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

/// `XREAD [COUNT n] [BLOCK ms] STREAMS <stream> <id|$>` -- v1 single stream
/// (Lite sessions consume one queue; multi-stream is future work).
pub async fn xread(ctx: &mut Ctx<'_>) {
    let Some((opts, mut i)) = parse_opts(&ctx.args, 0) else {
        return resp::append_error(ctx.out, "ERR syntax error");
    };
    if i >= ctx.args.len() || !ctx.args[i].eq_ignore_ascii_case(b"STREAMS") {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    i += 1;
    if ctx.args.len() != i + 2 {
        return resp::append_error(
            ctx.out,
            "ERR XREAD supports exactly one stream in Lite mode",
        );
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, i) else {
        return;
    };
    let id_arg = &ctx.args[i + 1];
    let after = if id_arg == b"$" {
        match model::read_meta(&ctx.shared.store, &prefix, &stream) {
            Ok(MetaRead::Live(m)) => m.last_id(),
            _ => model::MIN_ID,
        }
    } else {
        match model::parse_id(id_arg) {
            Some(id) => id,
            None => {
                return resp::append_error(
                    ctx.out,
                    "ERR Invalid stream ID specified as stream command argument",
                )
            }
        }
    };
    match opts.block_ms {
        None => {
            match entries::scan_entries(&ctx.shared.store, &prefix, &stream, after, opts.count) {
                Err(e) => resp::append_error(ctx.out, &format!("ERR: xread failed: {e}")),
                Ok(v) if v.is_empty() => nil_array(ctx.out),
                Ok(v) => {
                    monitor::observe_lite_message(&ctx.shared.monitor, "read", v.len() as u64);
                    append_read_reply(ctx.out, &stream, &v)
                }
            }
        }
        Some(ms) => {
            // Absolute deadline computed once so a signaled re-park
            // cannot reset the caller's BLOCK budget.
            let end = if ms == 0 {
                None
            } else {
                Instant::now().checked_add(Duration::from_millis(ms))
            };
            loop {
                let Some(budget) = remaining_ms(end, ms) else {
                    nil_array(ctx.out);
                    break;
                };
                match wait_entries(ctx, &prefix, &stream, after, opts.count, budget).await {
                    None => {
                        nil_array(ctx.out);
                        break;
                    }
                    Some(Err(e)) => {
                        resp::append_error(ctx.out, &format!("ERR: xread failed: {e}"));
                        break;
                    }
                    // An empty signaled wake (e.g. a group op notified the
                    // stream's meta key): nothing new for a plain XREAD,
                    // keep waiting for the remaining budget.
                    Some(Ok(v)) if v.is_empty() => continue,
                    Some(Ok(v)) => {
                        monitor::observe_lite_message(&ctx.shared.monitor, "read", v.len() as u64);
                        append_read_reply(ctx.out, &stream, &v);
                        break;
                    }
                }
            }
        }
    }
}

/// `XLEN <stream>`: retained entry count (0 for unknown streams).
pub async fn xlen(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xlen' command");
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, 0) else {
        return;
    };
    let len = match model::read_meta(&ctx.shared.store, &prefix, &stream) {
        Ok(MetaRead::Live(m)) => m.len,
        Ok(MetaRead::Purged) => {
            entries::count_reap(ctx);
            0
        }
        _ => 0,
    };
    resp::append_int(ctx.out, len as i64);
}

const NOGROUP_PREFIX: &str = "NOGROUP No such key";

fn nogroup(out: &mut Vec<u8>, stream: &[u8], group: &[u8]) {
    resp::append_error(
        out,
        &format!(
            "{} '{}' or consumer group '{}'",
            NOGROUP_PREFIX,
            String::from_utf8_lossy(stream),
            String::from_utf8_lossy(group)
        ),
    );
}

/// `XREADGROUP GROUP <g> <consumer> [COUNT n] [BLOCK ms] STREAMS <stream> >|<id>`
/// (`>` = new messages; an explicit id is a catch-up view that does not
/// move the delivery watermark).
pub async fn xreadgroup(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 6 || !ctx.args[0].eq_ignore_ascii_case(b"GROUP") {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    let (group, consumer) = (ctx.args[1].clone(), ctx.args[2].clone());
    let Some((opts, mut i)) = parse_opts(&ctx.args, 3) else {
        return resp::append_error(ctx.out, "ERR syntax error");
    };
    if i >= ctx.args.len() || !ctx.args[i].eq_ignore_ascii_case(b"STREAMS") {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    i += 1;
    if ctx.args.len() != i + 2 {
        return resp::append_error(
            ctx.out,
            "ERR XREADGROUP supports exactly one stream in Lite mode",
        );
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, i) else {
        return;
    };
    let id_arg = ctx.args[i + 1].clone();
    if id_arg == b"$" {
        return resp::append_error(
            ctx.out,
            "ERR The $ ID is meaningless in the context of this command",
        );
    }
    // Explicit id: catch-up view over the log; watermark untouched.
    if id_arg != b">" {
        let Some(explicit) = model::parse_id(&id_arg) else {
            return resp::append_error(
                ctx.out,
                "ERR Invalid stream ID specified as stream command argument",
            );
        };
        if offset::load(
            &ctx.shared.lite.offsets,
            &ctx.shared.store,
            &prefix,
            &stream,
            &group,
        )
        .ok()
        .flatten()
        .is_none()
        {
            return nogroup(ctx.out, &stream, &group);
        }
        return match entries::scan_entries(
            &ctx.shared.store,
            &prefix,
            &stream,
            explicit,
            opts.count,
        ) {
            Err(e) => resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}")),
            Ok(v) if v.is_empty() => nil_array(ctx.out),
            Ok(v) => append_read_reply(ctx.out, &stream, &v),
        };
    }

    // `>` mode: serialized read-advance under the stream latch; BLOCK parks
    // between attempts (never while holding the latch).
    let _ = consumer;
    let wake = model::meta_key(&prefix, &stream);
    // Absolute expiry for a bounded BLOCK; None when no bound is
    // computable -- no BLOCK at all, BLOCK 0 (forever), or a value too
    // large for Instant. wait_entries re-parks in slices itself, so no
    // clamped deadline is built here.
    let end = opts
        .block_ms
        .filter(|ms| *ms > 0)
        .and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms)));
    let mut after_snapshot;
    loop {
        {
            let _guard = crate::ds::latch::lock(&ctx.shared.latch, &wake).await;
            let Some(st) = offset::load(
                &ctx.shared.lite.offsets,
                &ctx.shared.store,
                &prefix,
                &stream,
                &group,
            )
            .ok()
            .flatten() else {
                return nogroup(ctx.out, &stream, &group);
            };
            after_snapshot = st.delivered;
            match entries::scan_entries(
                &ctx.shared.store,
                &prefix,
                &stream,
                st.delivered,
                opts.count,
            ) {
                Err(e) => {
                    return resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}"))
                }
                Ok(v) if !v.is_empty() => {
                    if let Some(last) = v.last().map(|e| e.id) {
                        offset::advance_delivered(&ctx.shared.lite.offsets, &stream, &group, last);
                    }
                    monitor::observe_lite_message(&ctx.shared.monitor, "read", v.len() as u64);
                    return append_read_reply(ctx.out, &stream, &v);
                }
                Ok(_) => {}
            }
        }
        let Some(block_ms) = opts.block_ms else {
            return nil_array(ctx.out);
        };
        // Budget still left to hand to wait_entries; 0 reaches it only
        // for BLOCK 0 (forever): a bounded wait whose expiry has passed
        // returns nil here, and a sub-millisecond remainder is clamped
        // to 1ms inside remaining_ms so it can never become "forever".
        let Some(left) = remaining_ms(end, block_ms) else {
            return nil_array(ctx.out);
        };
        match wait_entries(ctx, &prefix, &stream, after_snapshot, opts.count, left).await {
            None => return nil_array(ctx.out),
            Some(Err(e)) => {
                return resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}"))
            }
            // Data landed (latched quick path picks it up) OR a group op
            // signalled the meta key (DESTROY -> NOGROUP re-check, SETID
            // rewind -> replay): the loop head re-validates both.
            Some(Ok(_)) => continue,
        }
    }
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
        // wait_entries reads as BLOCK 0 = wait FOREVER; a bounded
        // XREADGROUP could hang past its deadline until an append woke
        // it. The remainder must clamp up to 1ms so the timeout fires.
        let sub = Instant::now() + Duration::from_micros(400);
        let got = remaining_ms(Some(sub), 5);
        assert_eq!(got, Some(1), "sub-millisecond budget must be 1ms, not 0");
    }
}
