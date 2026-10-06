//! Plain XREAD (no consumer groups): the id-anchored scan surface and
//! its BLOCK parking loop, split from [`super::read`] (which keeps the
//! shared STREAMS-list plumbing and the whole XREADGROUP machinery).
//! Streams that produced nothing are left out of the reply; an
//! across-the-board miss is the caller's nil array (the empty-stream
//! contract XREADGROUP honors too).

use std::time::{Duration, Instant};

use crate::command::Ctx;
use crate::monitor;
use crate::resp::codec as resp;

use super::entries;
use super::model::{self, EntryId, MetaRead};
use super::park_wait::{park_target, remaining_ms, wait_targets, ParkTarget};
use super::read::{
    append_streams_reply, never_gated, nil_array, parse_opts, spec_after, split_streams_tail,
    ReadId, StreamEntries, StreamSpec,
};

/// Scan every spec once, COUNT per stream; streams that produced
/// nothing are left out of the result (an across-the-board miss is the
/// caller's nil array). The first store error aborts the command.
fn scan_specs(
    store: &crate::store::Store,
    specs: &[StreamSpec],
    count: usize,
) -> Result<Vec<StreamEntries>, String> {
    let mut out = Vec::new();
    for s in specs {
        let v = entries::scan_entries(store, &s.prefix, &s.stream, spec_after(s), count)?;
        if !v.is_empty() {
            out.push((s.stream.clone(), v));
        }
    }
    Ok(out)
}
/// Per-stream id of an XREAD STREAMS list: `$` resolves to the named
/// stream's last_id snapshotted NOW (a later BLOCK waits only for
/// entries added after the command started); anything else must parse.
/// `None` = malformed id argument.
fn parse_read_id(ctx: &Ctx<'_>, id_arg: &[u8], prefix: &[u8], stream: &[u8]) -> Option<EntryId> {
    if id_arg == b"$" {
        match model::read_meta(
            &ctx.shared.store,
            prefix,
            stream,
            Some(ctx.shared.lite.as_ref()),
        ) {
            Ok(MetaRead::Live(m)) => Some(m.last_id()),
            _ => Some(model::MIN_ID),
        }
    } else {
        model::parse_id(id_arg)
    }
}

/// `XREAD [COUNT n] [BLOCK ms] STREAMS s1 s2... id1 id2...` -- read up
/// to COUNT entries per stream strictly past each stream's position
/// (`$` = that stream's last_id). Blocking parks one waiter under every
/// stream's meta key: the first XADD on any of them wins.
pub async fn xread(ctx: &mut Ctx<'_>) {
    let Some((opts, mut i)) = parse_opts(&ctx.args, 0) else {
        return resp::append_error(ctx.out, "ERR syntax error");
    };
    if i >= ctx.args.len() || !ctx.args[i].eq_ignore_ascii_case(b"STREAMS") {
        return resp::append_error(ctx.out, "ERR syntax error");
    }
    i += 1;
    let Some((id_start, n)) = split_streams_tail(&ctx.args, i) else {
        return resp::append_error(
            ctx.out,
            "ERR Unbalanced XREAD list of streams: for each stream key an ID or '$' must be specified.",
        );
    };
    // Resolve names first (a bad name replies its own error); then ids.
    let mut specs = Vec::with_capacity(n);
    for j in 0..n {
        let Some((stream, prefix)) = entries::stream_of(ctx, i + j) else {
            return;
        };
        let id_arg = ctx.args[id_start + j].clone();
        let Some(after) = parse_read_id(ctx, &id_arg, &prefix, &stream) else {
            return resp::append_error(
                ctx.out,
                "ERR Invalid stream ID specified as stream command argument",
            );
        };
        specs.push(StreamSpec {
            stream,
            prefix,
            id: ReadId::After(after),
        });
    }
    match opts.block_ms {
        None => match scan_specs(&ctx.shared.store, &specs, opts.count) {
            Err(e) => resp::append_error(ctx.out, &format!("ERR: xread failed: {e}")),
            Ok(results) => finish_xread(ctx, results),
        },
        Some(ms) => {
            // Absolute deadline computed once so a signaled re-park
            // cannot reset the caller's BLOCK budget.
            let end = if ms == 0 {
                None
            } else {
                Instant::now().checked_add(Duration::from_millis(ms))
            };
            let targets: Vec<ParkTarget> = specs
                .iter()
                .map(|s| park_target(s, spec_after(s), opts.count, false))
                .collect();
            loop {
                let Some(budget) = remaining_ms(end, ms) else {
                    nil_array(ctx.out);
                    break;
                };
                match wait_targets(ctx, &targets, budget, &never_gated).await {
                    None => {
                        nil_array(ctx.out);
                        break;
                    }
                    Some(Err(e)) => {
                        resp::append_error(ctx.out, &format!("ERR: xread failed: {e}"));
                        break;
                    }
                    // An empty signaled wake (e.g. a group op notified
                    // a stream's meta key): nothing new for a plain
                    // XREAD, keep waiting for the remaining budget.
                    Some(Ok(v)) if v.is_empty() => continue,
                    Some(Ok(v)) => {
                        finish_xread(ctx, v);
                        break;
                    }
                }
            }
        }
    }
}

/// XREAD reply tail: nothing found on any stream -> nil array;
/// otherwise one observation for the total served + the nested pairs.
fn finish_xread(ctx: &mut Ctx<'_>, results: Vec<StreamEntries>) {
    if results.is_empty() {
        return nil_array(ctx.out);
    }
    let total: u64 = results.iter().map(|(_, v)| v.len() as u64).sum();
    monitor::observe_lite_message(&ctx.shared.monitor, "read", total);
    append_streams_reply(ctx.out, &results);
}
