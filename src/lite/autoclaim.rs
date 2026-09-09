//! XAUTOCLAIM: cursor-driven scan that re-assigns idle pending
//! entries to one consumer (stuck-consumer take-over at scale). Shares
//! the PEL-rewrite helpers with [`super::claim`].

use crate::command::Ctx;
use crate::ds::expire;
use crate::ds::latch;
use crate::resp::codec as resp;

use super::claim::{
    claimed_state, group_absent, nogroup, parse_u64, read_entry, register_consumer, succ_id,
};
use super::entries;
use super::model::{self, EntryId};
use super::pel;

/// XAUTOCLAIM scans at most 10x the requested COUNT entries per call
/// (the Redis cap) before handing the cursor back, so one call cannot
/// grind through an unbounded PEL of busy rows.
const SCAN_BUDGET_FACTOR: u64 = 10;

/// `XAUTOCLAIM <stream> <group> <consumer> <min-idle-time> <start> [COUNT <n>] [JUSTID]`.
pub async fn xautoclaim(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 5 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xautoclaim' command",
        );
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, 0) else {
        return;
    };
    let (group, consumer) = (ctx.args[1].clone(), ctx.args[2].clone());
    let Some(min_idle) = parse_u64(&ctx.args[3]) else {
        return resp::append_error(ctx.out, "ERR value is not an integer or out of range");
    };
    // Cursor start: `0-0`-style ids, `-` (= 0-0) and `+` (start at the
    // end: claims nothing, cursor comes back 0-0).
    let start = match ctx.args[4].as_slice() {
        b"-" => model::MIN_ID,
        b"+" => model::MAX_ID,
        raw => match model::parse_id(raw) {
            Some(id) => id,
            None => return resp::append_error(ctx.out, "ERR Invalid stream ID specified"),
        },
    };
    let mut count: u64 = 100;
    let mut justid = false;
    let mut i = 5;
    while i < ctx.args.len() {
        if ctx.args[i].eq_ignore_ascii_case(b"COUNT") {
            i += 1;
            match ctx.args.get(i).and_then(|a| parse_u64(a)) {
                None => {
                    return resp::append_error(
                        ctx.out,
                        "ERR value is not an integer or out of range",
                    )
                }
                Some(0) => return resp::append_error(ctx.out, "ERR COUNT must be >= 1"),
                Some(n) => count = n,
            }
        } else if ctx.args[i].eq_ignore_ascii_case(b"JUSTID") {
            justid = true;
        } else {
            return resp::append_error(ctx.out, "ERR syntax error");
        }
        i += 1;
    }
    if group_absent(ctx, &prefix, &stream, &group) {
        return nogroup(ctx.out, &stream, &group);
    }
    let _guard = latch::lock(&ctx.shared.latch, &model::meta_key(&prefix, &stream)).await;
    if group_absent(ctx, &prefix, &stream, &group) {
        return nogroup(ctx.out, &stream, &group);
    }
    let rows = match pel::scan_pend(&ctx.shared.store, &prefix, &stream, &group, start, None) {
        Ok(rows) => rows,
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xautoclaim failed: {e}")),
    };
    let now = expire::now_ms();
    // Ordered groups: queue-granularity takeover -- only the PEL HEAD is
    // claimable (deeper rows stay with the owner until the new owner
    // works down to them). A successful head claim flips ownership and
    // bumps the fencing epoch; an empty result leaves ownership alone.
    let ordered = super::offset::load(
        &ctx.shared.lite.offsets,
        &ctx.shared.store,
        &prefix,
        &stream,
        &group,
    )
    .ok()
    .flatten()
    .is_some_and(|st| st.ordered);
    let head = if ordered {
        pel::scan_pend(
            &ctx.shared.store,
            &prefix,
            &stream,
            &group,
            model::MIN_ID,
            Some(1),
        )
        .ok()
        .and_then(|rs| rs.into_iter().next())
        .map(|row| row.id)
    } else {
        None
    };
    let mut epoch = 0u64;
    let mut batch = rocksdb::WriteBatch::default();
    register_consumer(ctx, &mut batch, &prefix, &stream, &group, &consumer, now);
    let budget = count.saturating_mul(SCAN_BUDGET_FACTOR);
    let (mut scanned, mut claimed) = (0u64, 0u64);
    let (mut last_scanned, mut stopped_early) = (None::<EntryId>, false);
    let mut frames: Vec<entries::Entry> = Vec::new();
    let mut claimed_ids: Vec<EntryId> = Vec::new();
    let mut deleted: Vec<EntryId> = Vec::new();
    for row in rows {
        if claimed >= count || scanned >= budget {
            stopped_early = true;
            break;
        }
        scanned += 1;
        last_scanned = Some(row.id);
        if ordered && Some(row.id) != head {
            // Beyond the (idle-gated) head: not claimable in ordered
            // mode; keep scanning so the cursor still advances.
            if claimed > 0 {
                break; // head claimed: stop right there
            }
            continue;
        }
        if now.saturating_sub(row.state.delivered_ms) < min_idle {
            continue; // not idle enough: keeps waiting with its owner
        }
        match read_entry(&ctx.shared.store, &prefix, &stream, row.id) {
            Err(e) => return resp::append_error(ctx.out, &format!("ERR: xautoclaim failed: {e}")),
            // Payload trimmed away: reap the orphan PEL row (reaped rows
            // do not count toward the claim target).
            Ok(None) => {
                batch.delete(pel::pend_key(&prefix, &stream, &group, row.id));
                deleted.push(row.id);
            }
            Ok(Some(fields)) => {
                if ordered && epoch == 0 {
                    // A successful head claim takes the queue: bump the
                    // fencing epoch over the deposed owner.
                    epoch = super::ordered::force_takeover(
                        &ctx.shared.lite.owners,
                        &stream,
                        &group,
                        &consumer,
                        now,
                    );
                }
                batch.put(
                    pel::pend_key(&prefix, &stream, &group, row.id),
                    pel::encode_pend(&claimed_state(
                        row.state.times_delivered,
                        false,
                        justid,
                        &consumer,
                        now,
                        epoch,
                    )),
                );
                if justid {
                    claimed_ids.push(row.id);
                } else {
                    frames.push(entries::Entry { id: row.id, fields });
                }
                claimed += 1;
            }
        }
    }
    if let Err(e) = ctx.commit(batch).await {
        return resp::append_error(ctx.out, &format!("ERR: xautoclaim failed: {e}"));
    }
    if ordered && epoch > 0 {
        // Takeover happened: wake blocked readers (the fenced former
        // owner and any contender) to re-check immediately.
        crate::ds::wait::notify(&ctx.shared.wait_hub, &model::meta_key(&prefix, &stream));
    }
    // Redis >= 7 shape: [next-cursor, claimed entries (or ids), deleted ids].
    // The cursor is the SUCCESSOR of the last scanned row: scanning is
    // inclusive of the start, so handing back the row itself would
    // re-claim it on the next call whenever the idle gate passes (a
    // min-idle-time of 0 always passes) -- a livelock of duplicate
    // deliveries. 0-0 when the scan ran off the end of the PEL.
    let cursor = match (stopped_early, last_scanned) {
        (true, Some(id)) => succ_id(id).unwrap_or(model::MAX_ID),
        _ => model::MIN_ID,
    };
    resp::append_array(ctx.out, 3);
    resp::append_bulk(ctx.out, model::format_id(cursor).as_bytes());
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
    resp::append_array(ctx.out, deleted.len());
    for id in &deleted {
        resp::append_bulk(ctx.out, model::format_id(*id).as_bytes());
    }
}
