//! Shared stream-entry primitives used by every Lite command: decoding
//! and scanning physical entries, framing them as RESP arrays, resolving
//! a command argument into a validated `(stream, slot-prefix)` pair, and
//! counting lazy idle-reclaims observed through meta reads.

use std::sync::atomic::Ordering;

use crate::command::Ctx;
use crate::hash;
use crate::resp::codec as resp;
use crate::store::{ops, Store};

use super::model::{self, EntryId};
use super::TopicName;

/// One decoded stream entry.
pub struct Entry {
    pub id: EntryId,
    pub fields: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Entry id from an entry physical key (`base` = the stream's entry_base).
pub(super) fn id_from_key(base: &[u8], k: &[u8]) -> Option<EntryId> {
    let sfx = k.get(base.len()..)?;
    if sfx.len() != 16 {
        return None;
    }
    Some(EntryId {
        ms: u64::from_be_bytes(sfx[..8].try_into().ok()?),
        seq: u64::from_be_bytes(sfx[8..].try_into().ok()?),
    })
}

/// Read up to `count` entries with id strictly greater than `after`.
pub fn scan_entries(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    after: EntryId,
    count: usize,
) -> Result<Vec<Entry>, String> {
    let base = model::entry_base(prefix, stream);
    let from = model::entry_key(prefix, stream, after);
    let mut out = Vec::new();
    ops::for_each_from(store, &from, true, &mut |k, v| {
        if !k.starts_with(&base) {
            return false; // left this stream's window
        }
        if let (Some(id), Some(fields)) = (id_from_key(&base, k), model::decode_entry(v)) {
            out.push(Entry { id, fields });
        }
        out.len() < count
    })?;
    Ok(out)
}

/// One entry as `[id, f1, v1, ...]`.
pub fn append_entry_frame(out: &mut Vec<u8>, e: &Entry) {
    resp::append_array(out, 2);
    resp::append_bulk(out, model::format_id(e.id).as_bytes());
    resp::append_array(out, e.fields.len() * 2);
    for (f, v) in &e.fields {
        resp::append_bulk(out, f);
        resp::append_bulk(out, v);
    }
}

/// Count a lazy idle-reclaim observed through a meta read.
pub(super) fn count_reap(ctx: &Ctx<'_>) {
    let stats = &ctx.shared.lite.stats;
    // Decrement with a floor at zero: two readers observing the same
    // purge (or a reap without a matching live count) must not drive the
    // gauge negative, so retry until a compare-and-swap lands in range.
    let mut cur = stats.streams_live.load(Ordering::Relaxed);
    while cur > 0 {
        match stats.streams_live.compare_exchange_weak(
            cur,
            cur - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(v) => cur = v,
        }
    }
    stats.streams_reaped.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn stream_of(ctx: &mut Ctx<'_>, i: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    match super::parse_topic_name(&ctx.args[i]) {
        Ok(TopicName::Stream(p, c)) => {
            let mut s = p.clone();
            s.push(b'/');
            s.extend_from_slice(&c);
            Some((s, hash::slot_with_prefix(&p).1))
        }
        Ok(TopicName::Parent(_)) => {
            resp::append_error(ctx.out, "ERR a full stream name 'parent/child' is required");
            None
        }
        Err(e) => {
            resp::append_error(ctx.out, &e);
            None
        }
    }
}
