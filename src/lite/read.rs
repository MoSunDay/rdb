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

use super::dlq;
use super::entries::{self, Entry};
use super::model::{self, EntryId, MetaRead};
use super::offset;
use super::ordered;
use super::park_wait::{park_target, remaining_ms, wait_targets, ParkTarget};
use super::pel;
use crate::monitor;
use crate::state;

pub(crate) struct ReadOpts {
    pub(crate) count: usize,
    pub(crate) block_ms: Option<u64>,
}

/// Parse `[COUNT n] [BLOCK ms]` starting at `i`; returns opts + next index.
pub(crate) fn parse_opts(args: &[Vec<u8>], mut i: usize) -> Option<(ReadOpts, usize)> {
    let mut opts = ReadOpts {
        count: 1000,
        block_ms: None,
    };
    while i < args.len() {
        if args[i].eq_ignore_ascii_case(b"COUNT") {
            let n = super::claim::parse_u64(args.get(i + 1)?)? as usize;
            if n == 0 {
                return None;
            }
            opts.count = n;
            i += 2;
        } else if args[i].eq_ignore_ascii_case(b"BLOCK") {
            opts.block_ms = Some(super::claim::parse_u64(args.get(i + 1)?)?);
            i += 2;
        } else {
            break;
        }
    }
    Some((opts, i))
}

pub(crate) fn nil_array(out: &mut Vec<u8>) {
    resp::append_raw(out, b"*-1\r\n");
}

/// One stream's contribution to a read reply: name plus entries
/// (always >= 1 -- streams with nothing are left out, and no stream
/// producing anything at all collapses to a nil array).
pub(crate) type StreamEntries = (Vec<u8>, Vec<Entry>);

// ---- STREAMS list ------------------------------------------------------

/// Per-stream read point of one STREAMS-list element.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ReadId {
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
    pub(crate) id: ReadId,
}

/// The fixed read position of a spec (XREAD specs and history specs are
/// always `After`; a stray `New` reads from the beginning).
pub(crate) fn spec_after(s: &StreamSpec) -> EntryId {
    match s.id {
        ReadId::After(a) => a,
        ReadId::New => model::MIN_ID,
    }
}

/// Split the tail after STREAMS into `(first-id index, stream count)`:
/// the first half of the remaining args names streams, the second half
/// holds one id per stream; an odd or empty tail is "Unbalanced" (the
/// parser cannot tell which stream lost its id).
pub(crate) fn split_streams_tail(args: &[Vec<u8>], i: usize) -> Option<(usize, usize)> {
    let n = args.len().checked_sub(i)?;
    if n == 0 || n % 2 != 0 {
        return None;
    }
    Some((i + n / 2, n / 2))
}

pub(crate) fn append_streams_reply(out: &mut Vec<u8>, results: &[StreamEntries]) {
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

/// `XLEN <stream>`: retained entry count (0 for unknown streams).
pub async fn xlen(ctx: &mut Ctx<'_>) {
    if ctx.args.len() != 1 {
        return resp::append_error(ctx.out, "ERR wrong number of arguments for 'xlen' command");
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, 0) else {
        return;
    };
    let len = match model::read_meta(
        &ctx.shared.store,
        &prefix,
        &stream,
        Some(ctx.shared.lite.as_ref()),
    ) {
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

/// Read-side gate for ORDERED `>` streams: could this consumer deliver
/// right now? Mirrors deliver_new's door -- in-flight window > 0 AND
/// ownership acquirable -- WITHOUT any of its side effects (no latch,
/// no acquire, no delivery), so a blocked reader can PARK on the closed
/// gate instead of spinning against it. `true` = gate closed: the
/// window is full or another consumer holds a fresh ownership lease.
fn ordered_gate_closed(
    shared: &state::Shared,
    s: &StreamSpec,
    group: &[u8],
    consumer: &[u8],
    st: &offset::GroupState,
) -> bool {
    if !st.ordered {
        return false;
    }
    // Same window arithmetic as deliver_new: pending counts live PEL
    // rows, acks free slots.
    if st.inflight_max.saturating_sub(st.pending) == 0 {
        return true;
    }
    !ordered::ownership_open(
        &shared.lite.owners,
        &s.stream,
        group,
        consumer,
        crate::ds::expire::now_ms(),
        shared.lite.lease_ms(),
    )
}

/// Cheap gate re-check for parked readers (runs after every waiter
/// registration): cached offset state and the owner map ONLY -- no
/// store IO, no latches -- so it never stalls the park loop. A missing
/// cache entry (the group was evicted by a reap/DESTROY) reads OPEN:
/// the loop head then re-validates the group authoritatively instead
/// of parking on a group that may be gone.
fn gate_open_cached(shared: &state::Shared, s: &StreamSpec, group: &[u8], consumer: &[u8]) -> bool {
    match offset::peek_cached(&shared.lite.offsets, &s.stream, group) {
        None => true,
        Some(st) => !ordered_gate_closed(shared, s, group, consumer, &st),
    }
}

/// Plain XREAD's park re-check: no group, no window, no owner -- its
/// targets are never gated, so the probe can never open.
pub(crate) fn never_gated(_: &StreamSpec) -> bool {
    false
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
pub(crate) fn succ_id(id: EntryId) -> Option<EntryId> {
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
    // MAXDELIVERY groups join the latch set with their DLQ target
    // (offset-cache peeks; the up-front validation warmed the cache).
    // DRIFT RE-VALIDATION: the DLQ names are peeked before the locks,
    // and a DESTROY + re-CREATE with a DIFFERENT DLQ can commit in
    // between -- writing the new target's window without its latch
    // would race every DLQ writer on it. A set that still covers the
    // re-peek under the lock is final (group config is frozen under
    // the stream latches); a drifted one is dropped and re-taken.
    let shared = ctx.shared;
    let dlq_of = |s: &StreamSpec| {
        dlq::group_dlq_target(offset::peek_cached(&shared.lite.offsets, &s.stream, group).as_ref())
    };
    let mut guards;
    loop {
        let dlq_names: Vec<Vec<u8>> = fresh.iter().filter_map(&dlq_of).collect();
        let mut keys: Vec<Vec<u8>> = fresh
            .iter()
            .map(|s| model::meta_key(&s.prefix, &s.stream))
            .collect();
        keys.extend(dlq_names.iter().filter_map(|n| dlq::dlq_latch_key(n)));
        keys.sort();
        keys.dedup();
        guards = Vec::with_capacity(keys.len());
        for k in &keys {
            guards.push(crate::ds::latch::lock(&shared.latch, k).await);
        }
        if fresh.iter().filter_map(&dlq_of).collect::<Vec<_>>() == dlq_names {
            break;
        }
        drop(guards); // stale set: re-peek and re-lock
    }
    let mut batch = rocksdb::WriteBatch::default();
    let mut results = Vec::new();
    let mut total: u64 = 0;
    let mut dlq_count: u64 = 0;
    let mut tally_sum = dlq::TransferTally::default();
    let mut marks: Vec<(Vec<u8>, Option<offset::RollbackMark>)> = Vec::new();
    let mut transferred: Vec<(Vec<u8>, Vec<u8>, Vec<u8>)> = Vec::new();
    let now_ms = crate::ds::expire::now_ms();
    for s in fresh {
        let Some(st) = group_state(ctx, s, group) else {
            rollback_marks(ctx.shared, group, &mut marks);
            return Err(DeliverErr::NoGroup(s.stream.clone()));
        };
        // Ordered groups: queue-exclusive ownership + in-flight cap.
        // The cap is the throughput knob -- 1 (default) is strict
        // serial (RocketMQ-orderly equivalent), a larger window is a
        // Kafka-style prefetch pipeline; a fenced-out or window-full
        // queue simply delivers nothing this round. A blocked reader
        // PARKS on that closed gate instead of spinning: such streams
        // park as gated targets woken by the stream's meta-key signals
        // (XADD / XACK / takeover / SETID / DESTROY) plus
        // lease-granular slices -- lease expiry never notifies -- and
        // re-probe the gate after every registration (see
        // ordered_gate_closed / park_wait).
        let mut cap = count;
        let mut epoch = 0u64;
        if st.ordered {
            let window = st.inflight_max.saturating_sub(st.pending);
            if window == 0 {
                continue; // in-flight window full: acks free slots
            }
            match super::ordered::acquire(
                &ctx.shared.lite.owners,
                &s.stream,
                group,
                consumer,
                now_ms,
                ctx.shared.lite.lease_ms(),
            ) {
                super::ordered::Access::Own {
                    epoch: live,
                    took_over,
                } => {
                    epoch = live;
                    if took_over {
                        // Ownership changed hands (or was freshly taken):
                        // wake parked contenders so a blocked would-be
                        // owner re-checks instead of sleeping out its
                        // BLOCK budget.
                        crate::ds::wait::notify(
                            &ctx.shared.wait_hub,
                            &model::meta_key(&s.prefix, &s.stream),
                        );
                    }
                    cap = (window as usize).min(count);
                }
                super::ordered::Access::Busy { .. } => continue, // fenced out
            }
        }
        let v = entries::scan_entries(&ctx.shared.store, &s.prefix, &s.stream, st.delivered, cap)
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
        // XCLAIM history survives a crash redelivery; only brand-new
        // ids grow the backlog counter.
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
        // MAXDELIVERY gate (dlq): over-cap re-deliveries transfer to the
        // DLQ instead of being served; fresh rows can never trip one.
        let (v, moved, mark) = match dlq::gate_delivery(
            dlq::transfer_ctx(ctx.shared, &mut batch, &s.prefix, &s.stream, group, &st),
            v,
            &already_pending,
            consumer,
            now_ms,
        ) {
            Ok(gated) => gated,
            Err(e) => {
                rollback_marks(ctx.shared, group, &mut marks);
                return Err(DeliverErr::Store(e));
            }
        };
        if moved.resolved > 0 {
            dlq_count += moved.resolved as u64;
            transferred.push((s.prefix.clone(), s.stream.clone(), st.dlq.clone()));
        }
        tally_sum.dlq_created += moved.dlq_created;
        tally_sum.moved += moved.moved;
        // Always snapshotted: even a keep-only round bumped the cached
        // pending backlog, which a failed commit must rewind too.
        marks.push((s.stream.clone(), mark));
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
                    epoch,
                }),
            );
        }
        // Registry row rides the batch EVERY delivery now (not just the
        // first sight): `seen_ms` is the idle-GC activity clock, and the
        // rewrite preserves the remembered `created_ms` -- one extra
        // small put inside the batch the PEL rows already sync, no new
        // commit and no new fsync per operation.
        let created = ctx
            .shared
            .lite
            .ensure_consumer(&s.stream, group, consumer, now_ms)
            .unwrap_or(now_ms);
        batch.put(
            pel::consumer_key(&s.prefix, &s.stream, group, consumer),
            pel::encode_consumer(&pel::ConsumerState {
                created_ms: created,
                seen_ms: now_ms,
            }),
        );
        total += v.len() as u64;
        // A dead-letter-only round delivers nothing: an empty entry
        // list would reply `[[stream, *0]]` -- an inner empty-array
        // marker that breaks the nil-for-empty-stream contract and
        // wakes BLOCK readers with an empty response. Drop it; the
        // transfers still commit (dlq_count) and the caller replies
        // nil or re-parks.
        if !v.is_empty() {
            results.push((s.stream.clone(), v));
        }
    }
    // A dead-letter-only round still commits: the transfers are it.
    if !results.is_empty() || dlq_count > 0 {
        if let Err(e) = ctx.commit(batch).await {
            // The watermark must never name ids the store never
            // resolved: rewind the mutated groups to their pre-batch
            // snapshots, so the entries stay pending/redeliverable.
            rollback_marks(ctx.shared, group, &mut marks);
            return Err(DeliverErr::Store(e));
        }
        // Accounting follows the commit (a void batch counts nothing).
        dlq::account_tally(ctx.shared, tally_sum);
        // One observation per command; a zero count is a no-op.
        monitor::observe_lite_message(&ctx.shared.monitor, "read", total);
        monitor::observe_lite_message(&ctx.shared.monitor, "dlq", dlq_count);
        // Transfers free window slots and grow the DLQ: wake readers.
        for (p, s, d) in &transferred {
            dlq::notify_transfer(ctx.shared, p, s, d);
        }
    }
    Ok(results)
}

/// Rewind every group a planned (partially planned or failed) delivery
/// batch touched back to its [`offset::RollbackMark`] snapshot: the
/// cached watermark/pending moves of a transfer must never survive a
/// store commit failure (the flusher would persist them; the poison
/// entries would be neither in the DLQ nor redeliverable).
fn rollback_marks(
    shared: &state::Shared,
    group: &[u8],
    marks: &mut Vec<(Vec<u8>, Option<offset::RollbackMark>)>,
) {
    for (stream, mark) in marks.drain(..) {
        offset::restore(&shared.lite.offsets, &stream, group, mark);
    }
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
            .map(|(idx, s)| {
                // Gated flags are recomputed EVERY iteration: the ack,
                // takeover or expiry that moves a gate between rounds
                // must not be cached stale across them.
                let gated = group_state(ctx, s, &group)
                    .is_some_and(|st| ordered_gate_closed(ctx.shared, s, &group, &consumer, &st));
                park_target(s, snapshots[idx], opts.count, gated)
            })
            .collect();
        if targets.is_empty() {
            return nil_array(ctx.out);
        }
        // Park-side gate probe: cache + owner-map reads only (see
        // gate_open_cached) -- wait_targets calls it after every waiter
        // registration, so it must never touch the store or latches.
        let shared = ctx.shared;
        let gate_open = |s: &StreamSpec| gate_open_cached(shared, s, &group, &consumer);
        // Parked-reader lease (idle-GC veto #2): while this command is
        // blocked in wait_targets, its consumer counts as an active
        // member of every `>` target and is never collected. Refcounted
        // acquire/release around the await, so every wake path (timeout,
        // error, data, gate) drops the count exactly once.
        for s in &fresh {
            ctx.shared.lite.park_acquire(&s.stream, &group, &consumer);
        }
        let woke = wait_targets(ctx, &targets, left, &gate_open).await;
        for s in &fresh {
            ctx.shared.lite.park_release(&s.stream, &group, &consumer);
        }
        match woke {
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
