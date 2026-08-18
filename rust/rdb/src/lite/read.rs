//! Consuming side: XREAD / XREADGROUP (+ XLEN). Blocking reads park on
//! the shared WaitHub via `spawn_blocking` (the hub is a sync Condvar);
//! XADD notifies the stream's meta key after its batch commits. XACK
//! lives in [`ack`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::command::Ctx;
use crate::ds::wait::{self, WaitOutcome};
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

/// Park until entries with id > `after` exist or `block_ms` elapses
/// (bounded by the deadline computed at entry). Registering the waiter
/// BEFORE the final read closes the lost-notify window against XADD.
async fn wait_entries(
    ctx: &mut Ctx<'_>,
    prefix: &[u8],
    stream: &[u8],
    after: EntryId,
    count: usize,
    block_ms: u64,
) -> Option<Result<Vec<Entry>, String>> {
    let wake = model::meta_key(prefix, stream);
    let deadline = Instant::now() + Duration::from_millis(block_ms.min(MAX_SLICE_MS));
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
        let now = Instant::now();
        if now >= deadline {
            wait::unregister(&ctx.shared.wait_hub, &wake, &waiter);
            return None;
        }
        let left = deadline - now;
        let w = Arc::clone(&waiter);
        let woke = tokio::task::spawn_blocking(move || wait::wait(&w, left)).await;
        wait::unregister(&ctx.shared.wait_hub, &wake, &waiter);
        match woke.unwrap_or(WaitOutcome::Timeout) {
            WaitOutcome::Timeout if Instant::now() >= deadline => return None,
            _ => {} // signaled or spurious: re-read
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
        Some(ms) => match wait_entries(ctx, &prefix, &stream, after, opts.count, ms).await {
            None => nil_array(ctx.out),
            Some(Err(e)) => resp::append_error(ctx.out, &format!("ERR: xread failed: {e}")),
            Some(Ok(v)) => {
                monitor::observe_lite_message(&ctx.shared.monitor, "read", v.len() as u64);
                append_read_reply(ctx.out, &stream, &v)
            }
        },
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
    let group_str = String::from_utf8_lossy(&group).to_string();
    let stream_str = String::from_utf8_lossy(&stream).to_string();

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
            &stream_str,
            &group_str,
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
    let deadline = opts
        .block_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms.min(MAX_SLICE_MS)));
    let mut after_snapshot;
    loop {
        {
            let _guard = crate::ds::latch::lock(&ctx.shared.latch, &wake).await;
            let Some(st) = offset::load(
                &ctx.shared.lite.offsets,
                &ctx.shared.store,
                &prefix,
                &stream_str,
                &group_str,
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
                        offset::advance_delivered(
                            &ctx.shared.lite.offsets,
                            &stream_str,
                            &group_str,
                            last,
                        );
                    }
                    monitor::observe_lite_message(&ctx.shared.monitor, "read", v.len() as u64);
                    return append_read_reply(ctx.out, &stream, &v);
                }
                Ok(_) => {}
            }
        }
        let Some(deadline) = deadline else {
            return nil_array(ctx.out);
        };
        let now = Instant::now();
        if now >= deadline {
            return nil_array(ctx.out);
        }
        let left = (deadline - now).as_millis() as u64;
        match wait_entries(ctx, &prefix, &stream, after_snapshot, opts.count, left).await {
            None => return nil_array(ctx.out),
            Some(Err(e)) => {
                return resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}"))
            }
            Some(Ok(_)) => continue, // data landed: the latched quick path picks it up
        }
    }
}
