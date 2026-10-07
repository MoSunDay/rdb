//! `XINFO STREAM <key> FULL [COUNT <n>]`: the Redis-7-shaped deep view
//! of one stream -- the tail entries plus every group with its full
//! PEL and consumer roster. Dispatch stays in [`super::info`]; this
//! module only builds the reply.
//!
//! Engine-model mapping (deviations documented in features/mq-lite.md,
//! never fabricated here): the meta record keeps no radix-tree
//! statistics and no entries-added / max-deleted-entry-id /
//! recorded-first-entry-id counters, so those Redis fields are OMITTED;
//! `seen-time` is the freshest PEL delivery of the consumer (registry
//! creation time when it has no pending history) -- the same
//! approximation `XINFO CONSUMERS` uses for idle.

use crate::command::Ctx;
use crate::ds::expire;
use crate::resp::codec as resp;
use crate::store::ops;

use super::entries::{append_entry_frame, id_from_key, Entry};
use super::group;
use super::model::{self, MetaRead};
use super::pel::{self, PendRow};

/// Default cap of the entries / PEL lists (Redis's own FULL default).
pub(crate) const DEFAULT_COUNT: usize = 10;

const NOT_INT: &str = "ERR value is not an integer or out of range";
const UNKNOWN: &str = "ERR Unknown subcommand or wrong number of arguments for 'xinfo' command";

/// Interpret the tokens after `XINFO STREAM <key>`: `Ok(None)` = the
/// plain (non-FULL) summary, `Ok(Some(n))` = FULL capped at `n`.
pub(crate) fn stream_args(rest: &[Vec<u8>]) -> Result<Option<usize>, &'static str> {
    match rest {
        [] => Ok(None),
        [f] if f.eq_ignore_ascii_case(b"FULL") => Ok(Some(DEFAULT_COUNT)),
        [f, c, n] if f.eq_ignore_ascii_case(b"FULL") && c.eq_ignore_ascii_case(b"COUNT") => {
            match parse_count(n) {
                Some(k) => Ok(Some(k)),
                None => Err(NOT_INT),
            }
        }
        _ => Err(UNKNOWN),
    }
}

fn parse_count(a: &[u8]) -> Option<usize> {
    let n: usize = std::str::from_utf8(a).ok()?.parse().ok()?;
    (n > 0).then_some(n)
}

/// The newest `count` entries, emitted oldest-first (Redis's FULL
/// order): a reverse walk from the id ceiling, flipped.
fn tail_entries(ctx: &Ctx<'_>, prefix: &[u8], stream: &[u8], count: usize) -> Vec<Entry> {
    let base = model::entry_base(prefix, stream);
    let from = model::entry_key(prefix, stream, model::MAX_ID);
    let mut entries = Vec::new();
    let _ = ops::for_each_down_from(&ctx.shared.store, &from, false, &mut |k, v| {
        if !k.starts_with(&base) {
            return false; // left this stream's window from above
        }
        if let (Some(id), Some(fields)) = (id_from_key(&base, k), model::decode_entry(v)) {
            entries.push(Entry { id, fields });
        }
        entries.len() < count
    });
    entries.reverse();
    entries
}

/// One group pending row as `[id, consumer, time-since-delivered,
/// delivery-counter]` (the Redis FULL shape).
fn append_pend_group_row(out: &mut Vec<u8>, row: &PendRow, now: u64) {
    resp::append_array(out, 4);
    resp::append_bulk(out, model::format_id(row.id).as_bytes());
    resp::append_bulk(out, &row.state.consumer);
    resp::append_int(out, now.saturating_sub(row.state.delivered_ms) as i64);
    resp::append_int(out, row.state.times_delivered as i64);
}

/// One consumer PEL row as `[id, time-since-delivered,
/// delivery-counter]` (the Redis FULL shape).
fn append_pend_consumer_row(out: &mut Vec<u8>, row: &PendRow, now: u64) {
    resp::append_array(out, 3);
    resp::append_bulk(out, model::format_id(row.id).as_bytes());
    resp::append_int(out, now.saturating_sub(row.state.delivered_ms) as i64);
    resp::append_int(out, row.state.times_delivered as i64);
}

/// One group's contribution: name, last-delivered-id, the pending list
/// (capped) and the consumer roster (each with its own capped pel).
fn append_group(
    ctx: &mut Ctx<'_>,
    stream: &[u8],
    prefix: &[u8],
    name: &[u8],
    count: usize,
    now: u64,
) -> Result<(), String> {
    let cached = super::offset::load(
        &ctx.shared.lite.offsets,
        &ctx.shared.store,
        prefix,
        stream,
        name,
    )?;
    // The cache may lag a flush behind the group record; fall back to
    // the record itself for the watermark field.
    let delivered = match cached {
        Some(st) => st.delivered,
        None => group::groups_of(&ctx.shared.store, prefix, stream)?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, p)| model::EntryId {
                ms: p.delivered_ms,
                seq: p.delivered_seq,
            })
            .unwrap_or(model::MIN_ID),
    };
    let rows = pel::scan_pend(&ctx.shared.store, prefix, stream, name, model::MIN_ID, None)?;
    let registry = pel::scan_consumers(&ctx.shared.store, prefix, stream, name)?;
    let mut names: Vec<Vec<u8>> = registry.iter().map(|c| c.name.clone()).collect();
    for row in &rows {
        names.push(row.state.consumer.clone());
    }
    names.sort();
    names.dedup();

    resp::append_array(ctx.out, 8);
    resp::append_bulk_string(ctx.out, "name");
    resp::append_bulk(ctx.out, name);
    resp::append_bulk_string(ctx.out, "last-delivered-id");
    resp::append_bulk(ctx.out, model::format_id(delivered).as_bytes());
    resp::append_bulk_string(ctx.out, "pending");
    resp::append_array(ctx.out, rows.len().min(count));
    for row in rows.iter().take(count) {
        append_pend_group_row(ctx.out, row, now);
    }
    resp::append_bulk_string(ctx.out, "consumers");
    resp::append_array(ctx.out, names.len());
    for cname in &names {
        let mine: Vec<&PendRow> = rows.iter().filter(|r| &r.state.consumer == cname).collect();
        let seen = mine
            .iter()
            .map(|r| r.state.delivered_ms)
            .max()
            .or_else(|| {
                registry
                    .iter()
                    .find(|c| &c.name == cname)
                    .map(|c| c.created_ms)
            })
            .unwrap_or(0);
        resp::append_array(ctx.out, 6);
        resp::append_bulk_string(ctx.out, "name");
        resp::append_bulk(ctx.out, cname);
        resp::append_bulk_string(ctx.out, "seen-time");
        resp::append_int(ctx.out, seen as i64);
        resp::append_bulk_string(ctx.out, "pending");
        resp::append_int(ctx.out, mine.len() as i64);
        resp::append_bulk_string(ctx.out, "pel");
        resp::append_array(ctx.out, mine.len().min(count));
        for row in mine.into_iter().take(count) {
            append_pend_consumer_row(ctx.out, row, now);
        }
    }
    Ok(())
}

fn append_id(out: &mut Vec<u8>, ms: u64, seq: u64) {
    resp::append_bulk(out, model::format_id(model::EntryId { ms, seq }).as_bytes());
}

/// `XINFO STREAM <key> FULL [COUNT <n>]` reply body.
pub(crate) fn stream_full(ctx: &mut Ctx<'_>, stream: &[u8], prefix: &[u8], count: usize) {
    let meta = match model::read_meta(
        &ctx.shared.store,
        prefix,
        stream,
        Some(ctx.shared.lite.as_ref()),
    ) {
        Ok(MetaRead::Live(m)) => m,
        Ok(_) => return resp::append_error(ctx.out, "ERR no such key"),
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xinfo failed: {e}")),
    };
    let groups = match group::groups_of(&ctx.shared.store, prefix, stream) {
        Ok(g) => g,
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xinfo failed: {e}")),
    };
    let entries = tail_entries(ctx, prefix, stream, count);
    let now = expire::now_ms();
    resp::append_array(ctx.out, 8);
    resp::append_bulk_string(ctx.out, "length");
    resp::append_int(ctx.out, meta.len as i64);
    resp::append_bulk_string(ctx.out, "last-generated-id");
    append_id(ctx.out, meta.last_ms, meta.last_seq);
    resp::append_bulk_string(ctx.out, "entries");
    resp::append_array(ctx.out, entries.len());
    for e in &entries {
        append_entry_frame(ctx.out, e);
    }
    resp::append_bulk_string(ctx.out, "groups");
    resp::append_array(ctx.out, groups.len());
    for (name, _) in &groups {
        if let Err(e) = append_group(ctx, stream, prefix, name, count, now) {
            return resp::append_error(ctx.out, &format!("ERR: xinfo failed: {e}"));
        }
    }
}
