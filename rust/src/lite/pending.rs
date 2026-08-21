//! XPENDING: two read-only views of a consumer group's PEL.
//!
//! Summary form (`XPENDING <stream> <group>`) aggregates the whole PEL:
//! total pending count, min/max pending ids, then flat consumer/count
//! pairs (Redis renders the consumer section inline after the three
//! summary fields; an empty PEL is just `[0, nil, nil]`).
//!
//! Range form (`[IDLE <ms>] <start> <end> <count> [<consumer>]`) walks
//! the id-ordered PEL window and returns at most `<count>` rows of
//! `[id, consumer, idle-ms, deliveries]`, filtered by consumer name and
//! minimum idle time. `-` / `+` / `(<id>` bounds follow XRANGE
//! (see [`model::parse_bound`]).

use std::collections::BTreeMap;

use crate::command::Ctx;
use crate::ds::expire;
use crate::resp::codec as resp;

use super::entries;
use super::model::{self, EntryId};
use super::offset;
use super::pel;

/// NOGROUP reply. Byte-identical twin of read.rs's private helper:
/// widening that one to `pub(crate)` would couple every PEL command to
/// read.rs, so each PEL-family file keeps its own copy.
fn nogroup(out: &mut Vec<u8>, stream: &[u8], group: &[u8]) {
    resp::append_error(
        out,
        &format!(
            "NOGROUP No such key '{}' or consumer group '{}'",
            String::from_utf8_lossy(stream),
            String::from_utf8_lossy(group)
        ),
    );
}

/// Decimal u64 option value (`IDLE <ms>`, `<count>`).
fn parse_u64(s: &[u8]) -> Option<u64> {
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// Whole-PEL aggregate for the summary reply. Rows arrive id-ordered
/// from [`pel::scan_pend`], so the first/last rows are the min/max ids;
/// consumer counts use a BTreeMap for a deterministic (byte-sorted)
/// consumer section.
fn summarize(rows: &[pel::PendRow]) -> (Option<EntryId>, Option<EntryId>, BTreeMap<Vec<u8>, u64>) {
    let mut per: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    for row in rows {
        *per.entry(row.state.consumer.clone()).or_insert(0) += 1;
    }
    (rows.first().map(|r| r.id), rows.last().map(|r| r.id), per)
}

/// Flat summary frame: `[total, min-id, max-id, consumer, count, ...]`;
/// nil id slots when the PEL is empty (no consumer section then).
fn append_summary(
    out: &mut Vec<u8>,
    total: usize,
    (min, max): (Option<EntryId>, Option<EntryId>),
    per: &BTreeMap<Vec<u8>, u64>,
) {
    if per.is_empty() {
        resp::append_array(out, 3);
        resp::append_int(out, 0);
        resp::append_null(out);
        resp::append_null(out);
        return;
    }
    resp::append_array(out, 3 + per.len() * 2);
    resp::append_int(out, total as i64);
    for id in [min, max] {
        match id {
            Some(id) => resp::append_bulk(out, model::format_id(id).as_bytes()),
            None => resp::append_null(out),
        }
    }
    for (name, n) in per {
        resp::append_bulk(out, name);
        resp::append_int(out, *n as i64);
    }
}

/// Range-form selection: bounds with exclusivity, optional consumer
/// filter and idle floor (`now - delivered >= idle`), capped at `count`
/// rows (0 = empty reply). Rows are id-ordered, so passing the end bound
/// ends the walk early.
fn select_rows(
    rows: Vec<pel::PendRow>,
    start: model::RangeBound,
    end: model::RangeBound,
    count: u64,
    consumer: Option<&[u8]>,
    idle_floor: Option<u64>,
    now: u64,
) -> Vec<pel::PendRow> {
    let mut out = Vec::new();
    for row in rows {
        if out.len() as u64 >= count {
            break;
        }
        if row.id < start.id || (start.excl && row.id == start.id) {
            continue;
        }
        if row.id > end.id || (end.excl && row.id == end.id) {
            break;
        }
        if let Some(c) = consumer {
            if row.state.consumer.as_slice() != c {
                continue;
            }
        }
        if let Some(ms) = idle_floor {
            if now.saturating_sub(row.state.delivered_ms) < ms {
                continue;
            }
        }
        out.push(row);
    }
    out
}

/// `XPENDING <stream> <group> [IDLE <ms>] <start> <end> <count> [<consumer>]`
/// -- summary when nothing follows the group.
pub async fn xpending(ctx: &mut Ctx<'_>) {
    if ctx.args.len() < 2 {
        return resp::append_error(
            ctx.out,
            "ERR wrong number of arguments for 'xpending' command",
        );
    }
    let Some((stream, prefix)) = entries::stream_of(ctx, 0) else {
        return;
    };
    let group = ctx.args[1].clone();
    // Optional IDLE filter, then the mandatory start/end/count triple.
    let mut i = 2;
    let mut idle_floor = None;
    if ctx.args.len() > 2 && ctx.args[2].eq_ignore_ascii_case(b"IDLE") {
        idle_floor = match ctx.args.get(3).and_then(|a| parse_u64(a)) {
            Some(ms) => Some(ms),
            None => {
                return resp::append_error(ctx.out, "ERR value is not an integer or out of range")
            }
        };
        i = 4;
    }
    let range = if ctx.args.len() == 2 {
        None
    } else {
        if ctx.args.len() < i + 3 {
            return resp::append_error(ctx.out, "ERR syntax error");
        }
        let (start, end) = match (
            model::parse_bound(&ctx.args[i]),
            model::parse_bound(&ctx.args[i + 1]),
        ) {
            (Some(s), Some(e)) => (s, e),
            _ => {
                return resp::append_error(
                    ctx.out,
                    "ERR Invalid stream ID specified as stream command argument",
                )
            }
        };
        let Some(count) = parse_u64(&ctx.args[i + 2]) else {
            return resp::append_error(ctx.out, "ERR value is not an integer or out of range");
        };
        if ctx.args.len() > i + 4 {
            return resp::append_error(ctx.out, "ERR syntax error");
        }
        let consumer = (ctx.args.len() == i + 4).then(|| ctx.args[i + 3].clone());
        Some((start, end, count, consumer))
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
    // Summary scans the whole PEL; the range scan starts at the start
    // bound's id and filters (exclusivity/consumer/idle/count) locally,
    // since a raw scan limit would miscount filtered-out rows.
    let from = range.as_ref().map_or(model::MIN_ID, |(start, ..)| start.id);
    let rows = match pel::scan_pend(&ctx.shared.store, &prefix, &stream, &group, from, None) {
        Ok(rows) => rows,
        Err(e) => return resp::append_error(ctx.out, &format!("ERR: xpending failed: {e}")),
    };
    match range {
        None => {
            let (min, max, per) = summarize(&rows);
            append_summary(ctx.out, rows.len(), (min, max), &per);
        }
        Some((start, end, count, consumer)) => {
            let now = expire::now_ms();
            let rows = select_rows(
                rows,
                start,
                end,
                count,
                consumer.as_deref(),
                idle_floor,
                now,
            );
            resp::append_array(ctx.out, rows.len());
            for row in &rows {
                resp::append_array(ctx.out, 4);
                resp::append_bulk(ctx.out, model::format_id(row.id).as_bytes());
                resp::append_bulk(ctx.out, &row.state.consumer);
                resp::append_int(ctx.out, now.saturating_sub(row.state.delivered_ms) as i64);
                resp::append_int(ctx.out, row.state.times_delivered as i64);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ms: u64, seq: u64, consumer: &[u8], delivered_ms: u64, times: u64) -> pel::PendRow {
        pel::PendRow {
            id: EntryId { ms, seq },
            state: pel::PendState {
                consumer: consumer.to_vec(),
                delivered_ms,
                times_delivered: times,
            },
        }
    }

    fn bound(ms: u64, seq: u64, excl: bool) -> model::RangeBound {
        model::RangeBound {
            id: EntryId { ms, seq },
            excl,
        }
    }

    /// The fixture rows, rebuilt per call: PendRow is not Clone, and the
    /// point of the filter test is that each argument narrows the same
    /// three-row PEL differently.
    fn fixture() -> Vec<pel::PendRow> {
        vec![
            row(5, 0, b"a", 100, 1),
            row(5, 1, b"b", 600, 2),
            row(6, 0, b"a", 900, 1),
        ]
    }

    #[test]
    fn select_rows_applies_bounds_consumer_idle_and_cap() {
        let start = bound(5, 0, false);
        let end = bound(6, 0, true); // exclusive of 6-0
                                     // count=0 caps to an empty reply without touching the rows.
        assert!(select_rows(fixture(), start, end, 0, None, None, 1000).is_empty());
        // inclusive bounds + idle floor 500 (now 1000) drops 5-1 (idle 400).
        let got = select_rows(fixture(), start, end, 10, None, Some(500), 1000);
        assert_eq!(
            got.iter().map(|r| (r.id.ms, r.id.seq)).collect::<Vec<_>>(),
            vec![(5, 0)]
        );
        // consumer filter keeps only b's row once the idle floor is gone.
        let got = select_rows(
            fixture(),
            bound(5, 1, false),
            end,
            10,
            Some(b"b"),
            None,
            1000,
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].state.consumer, b"b".to_vec());
        // cap: only the first matching row comes back.
        let got = select_rows(fixture(), start, end, 1, None, None, 1000);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn summarize_takes_min_max_from_scan_order() {
        let rows = vec![
            row(5, 0, b"a", 100, 1),
            row(6, 0, b"a", 100, 1),
            row(7, 0, b"b", 100, 3),
        ];
        let (min, max, per) = summarize(&rows);
        assert_eq!(min, Some(EntryId { ms: 5, seq: 0 }));
        assert_eq!(max, Some(EntryId { ms: 7, seq: 0 }));
        assert_eq!(per.get(b"a".as_slice()), Some(&2));
        assert_eq!(per.get(b"b".as_slice()), Some(&1));
        // The consumer section is byte-sorted for a deterministic reply.
        assert_eq!(
            per.keys().cloned().collect::<Vec<_>>(),
            vec![b"a".to_vec(), b"b".to_vec()]
        );
        let (min, max, per) = summarize(&[]);
        assert_eq!((min, max), (None, None));
        assert!(per.is_empty());
    }
}
