//! Consuming side: XREAD / XREADGROUP (+ XLEN). Both read commands take
//! a multi-stream STREAMS list (first half stream names, second half
//! ids); blocking reads park ONE waiter registered under every distinct
//! stream meta key via the dedicated park pool (see [`park_wait`]); XADD notifies both the
//! stream's and its parent topic's meta keys after its batch commits.
//! XREADGROUP `>` delivery hands entries to the NAMED consumer and
//! registers their PEL rows (see [`pel`]); explicit ids serve that
//! consumer's PEL history. XACK lives in [`ack`].

use std::time::{Duration, Instant};

use crate::command::Ctx;
use crate::resp::codec as resp;
use crate::store::ops;

use super::entries::{self, Entry};
use super::model::{self, EntryId, MetaRead};
use super::offset;
use super::park_wait::{park_target, remaining_ms, wait_targets, ParkTarget};
use super::pel;
use crate::monitor;

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

/// One stream's contribution to a read reply: name plus entries
/// (always >= 1 -- streams with nothing are left out, and no stream
/// producing anything at all collapses to a nil array).
pub(crate) type StreamEntries = (Vec<u8>, Vec<Entry>);

// ---- STREAMS list ------------------------------------------------------

/// Per-stream read point of one STREAMS-list element.
#[derive(Clone, Copy, PartialEq)]
enum ReadId {
    /// `>`: deliver entries past the group's delivered watermark.
    New,
    /// A fixed exclusive start: XREAD's read position (a resolved `$`
    /// too) or XREADGROUP's PEL-history start.
    After(EntryId),
}

/// One parsed STREAMS-list element.
#[derive(Clone)]
pub(crate) struct StreamSpec {
    pub(crate) stream: Vec<u8>,
    pub(crate) prefix: Vec<u8>,
    id: ReadId,
}

/// The fixed read position of a spec (XREAD specs and history specs are
/// always `After`; a stray `New` reads from the beginning).
fn spec_after(s: &StreamSpec) -> EntryId {
    match s.id {
        ReadId::After(a) => a,
        ReadId::New => model::MIN_ID,
    }
}

/// Split the tail after STREAMS into `(first-id index, stream count)`:
/// the first half of the remaining args names streams, the second half
/// holds one id per stream; an odd or empty tail is "Unbalanced" (the
/// parser cannot tell which stream lost its id).
fn split_streams_tail(args: &[Vec<u8>], i: usize) -> Option<(usize, usize)> {
    let n = args.len().checked_sub(i)?;
    if n == 0 || n % 2 != 0 {
        return None;
    }
    Some((i + n / 2, n / 2))
}

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

fn append_streams_reply(out: &mut Vec<u8>, results: &[StreamEntries]) {
    resp::append_array(out, results.len());
    for (name, entries) in results {
        resp::append_array(out, 2);
        resp::append_bulk(out, name);
        resp::append_array(out, entries.len());
        for e in entries {
            entries::append_entry_frame(out, e);
        }
    }
}

// ---- XREAD -------------------------------------------------------------

/// Per-stream id of an XREAD STREAMS list: `$` resolves to the named
/// stream's last_id snapshotted NOW (a later BLOCK waits only for
/// entries added after the command started); anything else must parse.
/// `None` = malformed id argument.
fn parse_read_id(ctx: &Ctx<'_>, id_arg: &[u8], prefix: &[u8], stream: &[u8]) -> Option<EntryId> {
    if id_arg == b"$" {
        match model::read_meta(&ctx.shared.store, prefix, stream) {
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
                .map(|s| park_target(s, spec_after(s), opts.count))
                .collect();
            loop {
                let Some(budget) = remaining_ms(end, ms) else {
                    nil_array(ctx.out);
                    break;
                };
                match wait_targets(ctx, &targets, budget).await {
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

// ---- XREADGROUP --------------------------------------------------------

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

/// Cached group state of one (stream, group); `None` when the group
/// does not exist (a store error degrades to a miss -- NOGROUP is the
/// safest visible outcome, same as the pre-split code).
fn group_state(ctx: &Ctx<'_>, s: &StreamSpec, group: &[u8]) -> Option<offset::GroupState> {
    let cache = &ctx.shared.lite.offsets;
    offset::load(cache, &ctx.shared.store, &s.prefix, &s.stream, group)
        .ok()
        .flatten()
}

/// Per-stream id of an XREADGROUP STREAMS list: `>` (new deliveries)
/// or an explicit id (PEL history start). `$` is meaningless here and
/// bad ids get Redis's wording.
fn parse_group_id(id_arg: &[u8]) -> Result<ReadId, &'static str> {
    if id_arg == b">" {
        Ok(ReadId::New)
    } else if id_arg == b"$" {
        Err("ERR The $ ID is meaningless in the context of this command")
    } else {
        model::parse_id(id_arg)
            .map(ReadId::After)
            .ok_or("ERR Invalid stream ID specified as stream command argument")
    }
}

/// Why a delivery attempt bailed: a store/commit error (generic ERR
/// reply) or a group that vanished while parked (NOGROUP, per stream).
enum DeliverErr {
    Store(String),
    NoGroup(Vec<u8>),
}

/// Smallest id strictly greater than `id`; `None` at the id ceiling
/// (`<u64::MAX, u64::MAX>` -- nothing can ever follow it).
fn succ_id(id: EntryId) -> Option<EntryId> {
    match (id.seq < u64::MAX, id.ms < u64::MAX) {
        // Intra-millisecond: seq bumps; an exhausted seq rolls the ms.
        (true, _) => Some(EntryId {
            seq: id.seq + 1,
            ..id
        }),
        (false, true) => Some(EntryId {
            ms: id.ms + 1,
            seq: 0,
        }),
        // The id ceiling: nothing can ever follow.
        (false, false) => None,
    }
}

/// This consumer's PEL history past `after`: rows owned by THIS
/// consumer only (XCLAIM is the transfer path), joined back to their
/// log entries (dangling rows skipped). Never blocks, never mutates.
fn read_history(
    ctx: &Ctx<'_>,
    s: &StreamSpec,
    group: &[u8],
    consumer: &[u8],
    after: EntryId,
    count: usize,
) -> Result<Vec<Entry>, String> {
    // scan_pend's `from` is INCLUSIVE (XPENDING range semantics) while
    // the explicit id is an EXCLUSIVE start -- probe from its successor
    // or the entry the caller last processed would come straight back.
    // (The limit is a row cap applied before the consumer filter, like
    // every bounded PEL walk.)
    let Some(from) = succ_id(after) else {
        return Ok(Vec::new());
    };
    let rows = pel::scan_pend(
        &ctx.shared.store,
        &s.prefix,
        &s.stream,
        group,
        from,
        Some(count),
    )?;
    let mut out = Vec::new();
    for row in rows {
        if row.state.consumer != consumer {
            continue;
        }
        // Dangling receipt (log entry trimmed / stream deleted): the
        // PEL/entry reconciliation drops the row; nothing to serve.
        let Some(raw) = ops::get_physical(
            &ctx.shared.store,
            &model::entry_key(&s.prefix, &s.stream, row.id),
        )?
        else {
            continue;
        };
        if let Some(fields) = model::decode_entry(&raw) {
            out.push(Entry { id: row.id, fields });
        }
    }
    Ok(out)
}

/// Deliver new entries of every `>` stream to `consumer`. All their
/// meta latches are taken AT ONCE in byte-sorted key order (the shared
/// deadlock convention, see `flush_offsets_once`) and held across the
/// awaited commit: under the guards each stream re-loads its watermark
/// (NOGROUP re-check -- XGROUP DESTROY may have committed while we were
/// parked), scans past it, advances the watermark, bumps the pending
/// backlog, and adds its PEL rows plus -- first sight of the consumer
/// only -- the registry row to ONE batch committed once: a crash either
/// records a delivery whole or not at all, and watermark + PEL rows can
/// never disagree on disk.
async fn deliver_new(
    ctx: &mut Ctx<'_>,
    fresh: &[StreamSpec],
    group: &[u8],
    consumer: &[u8],
    count: usize,
) -> Result<Vec<StreamEntries>, DeliverErr> {
    let mut keys: Vec<Vec<u8>> = fresh
        .iter()
        .map(|s| model::meta_key(&s.prefix, &s.stream))
        .collect();
    keys.sort();
    keys.dedup();
    let mut guards = Vec::with_capacity(keys.len());
    for k in &keys {
        guards.push(crate::ds::latch::lock(&ctx.shared.latch, k).await);
    }
    let mut batch = rocksdb::WriteBatch::default();
    let mut results = Vec::new();
    let mut total: u64 = 0;
    let now_ms = crate::ds::expire::now_ms();
    for s in fresh {
        let Some(st) = group_state(ctx, s, group) else {
            return Err(DeliverErr::NoGroup(s.stream.clone()));
        };
        let v = entries::scan_entries(&ctx.shared.store, &s.prefix, &s.stream, st.delivered, count)
            .map_err(DeliverErr::Store)?;
        if v.is_empty() {
            continue;
        }
        // Watermark forward + backlog +n in memory: the 200ms flusher
        // persists the watermark; the PEL rows are durable right here.
        if let Some(last) = v.last().map(|e| e.id) {
            offset::advance_delivered(&ctx.shared.lite.offsets, &s.stream, group, last);
        }
        // Rows already pending (a rewind re-delivery: restart to the
        // committed watermark, XGROUP SETID back) are re-OWNED by this
        // reader with their delivery count carried over and bumped --
        // XCLAIM-accumulated history survives a crash redelivery -- but
        // only brand-new ids grow the backlog counter.
        let already_pending: std::collections::HashMap<EntryId, u64> = pel::scan_pend(
            &ctx.shared.store,
            &s.prefix,
            &s.stream,
            group,
            st.delivered,
            Some(v.len()),
        )
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.id, row.state.times_delivered))
        .collect();
        let fresh_rows = v
            .iter()
            .filter(|e| !already_pending.contains_key(&e.id))
            .count() as u64;
        offset::bump_pending(
            &ctx.shared.lite.offsets,
            &s.stream,
            group,
            fresh_rows as i64,
        );
        for e in &v {
            let times = already_pending
                .get(&e.id)
                .map_or(1, |old| old.saturating_add(1));
            batch.put(
                pel::pend_key(&s.prefix, &s.stream, group, e.id),
                pel::encode_pend(&pel::PendState {
                    consumer: consumer.to_vec(),
                    delivered_ms: now_ms,
                    times_delivered: times,
                }),
            );
        }
        // First sight of this consumer: registry row rides the batch.
        if !ctx.shared.lite.ensure_consumer(&s.stream, group, consumer) {
            batch.put(
                pel::consumer_key(&s.prefix, &s.stream, group, consumer),
                pel::encode_consumer(&pel::ConsumerState { created_ms: now_ms }),
            );
        }
        total += v.len() as u64;
        results.push((s.stream.clone(), v));
    }
    if !results.is_empty() {
        ctx.commit(batch).await.map_err(DeliverErr::Store)?;
        // One observation per command: delivered entries only (history
        // reads are local views, not deliveries).
        monitor::observe_lite_message(&ctx.shared.monitor, "read", total);
    }
    Ok(results)
}

/// Weave per-stream results back into the caller's STREAMS-list order
/// (`>` deliveries and history are collected apart).
fn merge_results(
    specs: &[StreamSpec],
    mut fresh: Vec<StreamEntries>,
    mut history: Vec<StreamEntries>,
) -> Vec<StreamEntries> {
    let mut merged = Vec::new();
    for s in specs {
        if let Some(pos) = fresh.iter().position(|(n, _)| n == &s.stream) {
            merged.push(fresh.remove(pos));
        } else if let Some(pos) = history.iter().position(|(n, _)| n == &s.stream) {
            merged.push(history.remove(pos));
        }
    }
    merged
}

/// `XREADGROUP GROUP <g> <consumer> [COUNT n] [BLOCK ms] STREAMS
/// s1 s2... id1 id2...` -- each id is `>` (deliver new entries to
/// `<consumer>` + register their PEL rows) or explicit (serve this
/// consumer's PEL history; `$` is meaningless here and rejected).
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
    let Some((id_start, n)) = split_streams_tail(&ctx.args, i) else {
        return resp::append_error(
            ctx.out,
            "ERR Unbalanced XREADGROUP list of streams: for each stream key an ID or '$' must be specified.",
        );
    };
    let mut specs = Vec::with_capacity(n);
    for j in 0..n {
        let Some((stream, prefix)) = entries::stream_of(ctx, i + j) else {
            return;
        };
        let id_arg = &ctx.args[id_start + j];
        let id = match parse_group_id(id_arg) {
            Ok(id) => id,
            Err(msg) => return resp::append_error(ctx.out, msg),
        };
        specs.push(StreamSpec { stream, prefix, id });
    }
    // Up-front validation BEFORE any latch or scan: every named group
    // must exist (first miss wins), so a typo'd stream fails fast
    // instead of half-delivering the others first.
    for s in &specs {
        if group_state(ctx, s, &group).is_none() {
            return nogroup(ctx.out, &s.stream, &group);
        }
    }
    // `>` streams deliver under their latches; explicit ids read this
    // consumer's history. Blocking parks only on the `>` streams --
    // history is fully served by the loop head.
    let fresh: Vec<StreamSpec> = specs
        .iter()
        .filter(|s| s.id == ReadId::New)
        .cloned()
        .collect();
    // Absolute expiry for a bounded BLOCK; None when no bound is
    // computable -- no BLOCK at all, BLOCK 0 (forever), or a value too
    // large for Instant (wait_targets re-parks in slices itself).
    let end = opts
        .block_ms
        .filter(|ms| *ms > 0)
        .and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms)));
    // Wake probes track each `>` stream's last-seen watermark so the
    // pre-park data check can tell a real append from a spurious or
    // group-op signal (refreshed after each delivery round).
    let mut snapshots: Vec<EntryId> = fresh
        .iter()
        .map(|s| group_state(ctx, s, &group).map_or(model::MIN_ID, |st| st.delivered))
        .collect();
    loop {
        // History first: never blocks, never mutates, and any hit
        // alone already completes the reply.
        let mut history = Vec::new();
        for s in specs.iter().filter(|s| s.id != ReadId::New) {
            match read_history(ctx, s, &group, &consumer, spec_after(s), opts.count) {
                Err(e) => {
                    return resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}"))
                }
                Ok(v) if !v.is_empty() => history.push((s.stream.clone(), v)),
                Ok(_) => {}
            }
        }
        match deliver_new(ctx, &fresh, &group, &consumer, opts.count).await {
            Err(DeliverErr::NoGroup(stream)) => return nogroup(ctx.out, &stream, &group),
            Err(DeliverErr::Store(e)) => {
                return resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}"))
            }
            Ok(delivered) => {
                // Refresh each delivered stream's wake probe: the last
                // delivered id IS the advanced watermark.
                for (idx, s) in fresh.iter().enumerate() {
                    if let Some(last) = delivered
                        .iter()
                        .find(|(n, _)| n == &s.stream)
                        .and_then(|(_, v)| v.last())
                    {
                        snapshots[idx] = last.id;
                    }
                }
                let results = merge_results(&specs, delivered, history);
                if !results.is_empty() {
                    return append_streams_reply(ctx.out, &results);
                }
            }
        }
        let Some(block_ms) = opts.block_ms else {
            return nil_array(ctx.out);
        };
        // Budget still left to hand to wait_targets; 0 reaches it only
        // for BLOCK 0 (forever) -- a bounded wait whose expiry passed
        // returns nil here, and remaining_ms clamps a sub-millisecond
        // remainder up to 1ms so it can never become "forever".
        let Some(left) = remaining_ms(end, block_ms) else {
            return nil_array(ctx.out);
        };
        // Explicit-id-only reads never park: history was just served
        // (empty) and waiting on `>` streams cannot grow it.
        let targets: Vec<ParkTarget> = fresh
            .iter()
            .enumerate()
            .map(|(idx, s)| park_target(s, snapshots[idx], opts.count))
            .collect();
        if targets.is_empty() {
            return nil_array(ctx.out);
        }
        match wait_targets(ctx, &targets, left).await {
            None => return nil_array(ctx.out),
            Some(Err(e)) => {
                return resp::append_error(ctx.out, &format!("ERR: xreadgroup failed: {e}"))
            }
            // Data landed (latched quick path picks it up) OR a group op
            // signalled a meta key (DESTROY -> NOGROUP re-check, SETID
            // rewind -> replay): the loop head re-validates both.
            Some(Ok(_)) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_streams_tail_pairs_each_name_with_one_id() {
        let arg = |s: &str| s.as_bytes().to_vec();
        // Two streams, two ids: the id half starts after the name half.
        let args = vec![arg("orders/q0"), arg("orders/q1"), arg("$"), arg("5-0")];
        assert_eq!(split_streams_tail(&args, 0), Some((2, 2)));
        let with_opts = vec![arg("COUNT"), arg("10"), arg("a/b"), arg("0-0")];
        assert_eq!(split_streams_tail(&with_opts, 2), Some((3, 1)));
        // Odd tail (a stream lost its id) and an empty tail are both
        // "Unbalanced": the caller cannot pair names to ids.
        assert_eq!(split_streams_tail(&args[..3], 0), None);
        assert_eq!(split_streams_tail(&args[..0], 0), None);
    }

    #[test]
    fn succ_id_steps_seq_then_ms() {
        let id = |ms: u64, seq: u64| model::EntryId { ms, seq };
        // Intra-millisecond: seq bumps; seq exhausted rolls into the
        // next millisecond; the id ceiling has no successor.
        assert_eq!(succ_id(id(5, 1)), Some(id(5, 2)));
        assert_eq!(succ_id(id(5, u64::MAX)), Some(id(6, 0)));
        assert_eq!(succ_id(id(u64::MAX, u64::MAX)), None);
    }
}
