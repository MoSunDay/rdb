//! Staged delayed messages (WP2, `XADD ... DELAY <ms>`): the write side
//! parks the message body in a kind-0x1D staging row that is NOT a
//! stream entry, so every read path (XREAD/XREADGROUP/XRANGE/XLEN) is
//! physically blind to it until due; the due sweep then exchanges the
//! row into the target stream as a plain entry -- after that, PEL,
//! XCLAIM, XACK, ordered groups and the kafka surface treat it like any
//! other message (no consumer-side concept of "delayed").
//!
//! Layout + lifecycle (see `ds::codec::KIND_STREAM_DELAY`):
//! - row key = `<slot_prefix> 0x1D <due_ms BE> <stream_len BE> <stream>
//!   <locked id 16B>`; row value = `model::encode_entry` body. Keys are
//!   DUE-MAJOR, so the sweep is a bounded early-stopping prefix scan of
//!   each slot's 0x1D window (the first row with `due > now` ends the
//!   window walk -- every later key in that window is due-later too).
//! - XADD reserves an id under the stream latch and advances the meta
//!   `last_id` in the SAME commit as the staging row (no later plain
//!   XADD can collide with the reservation); `len` is NOT bumped, so
//!   XLEN keeps excluding staged rows.
//! - At due, under the same per-stream latch, one WriteBatch holds
//!   every row delete + every entry write + the meta maintenance: the
//!   exchange is a single WAL point, so a kill -9 either leaves the row
//!   (re-exchanged after restart -- no loss) or the entry (never
//!   re-exchanged -- no duplicate).
//!
//! ID POLICY (documented deviation from the WP2 sketch): the plan drew
//! "reuse the locked id at exchange", but a locked id can end up BELOW
//! a group's delivered watermark (or an XREAD `$` position) when plain
//! traffic appended and delivered newer ids during the delay window --
//! such an entry would be invisible to `>` readers forever, silently
//! LOSING the message on any busy stream. The exchange therefore
//! allocates a FRESH id (exactly like a plain append), which also makes
//! the plan's own "consumer sees due order, ids monotonic" guarantee
//! hold: out-of-order submissions exchange in due order and read back
//! in due order. The XADD reply keeps the reserved id (unique, ordered,
//! truthful at write time); it is a reservation token, not a promise
//! that an entry with that exact id will exist.

use std::sync::Arc;
use std::time::Duration;

use rocksdb::WriteBatch;

use crate::ds::{codec, expire, latch, wait};
use crate::state;
use crate::store::ops;

use super::model::{self, EntryId, MetaRead};

/// Rows exchanged per sweep round; leftovers stay staged and ride the
/// next round (the sweep is stateless and rescans from the head).
pub const ROUND_BUDGET: usize = 128;

/// Stage one delayed message into `batch` (the same commit that carries
/// the XADD reply's meta maintenance). `locked` is the reserved id.
pub(crate) fn stage_row(
    batch: &mut WriteBatch,
    prefix: &[u8],
    stream: &[u8],
    locked: EntryId,
    due_ms: u64,
    fpairs: &[(&[u8], &[u8])],
) {
    batch.put(
        codec::delay_row_key(prefix, due_ms, stream, (locked.ms, locked.seq)),
        model::encode_entry(fpairs),
    );
}

/// One staged row located by the due scan (physical key kept for the
/// exchange batch's delete).
struct DueRow {
    key: Vec<u8>,
    stream: Vec<u8>,
    fields: Vec<(Vec<u8>, Vec<u8>)>,
}

/// One due-sweep round: walk every slot's 0x1D window with an
/// early-stopping prefix scan (`due <= now` only), then exchange each
/// victim stream's rows under its latch in one batch. Returns
/// `(exchanged, dropped)` -- `dropped` counts rows deleted WITHOUT an
/// exchange (orphaned on a deleted/renamed-away stream). Rows whose
/// VALUE does not decode are neither exchanged nor deleted (counted in
/// `dropped` for observability but left in place: an undecodable row is
/// never silently destroyed; it cannot block the scan, whose early stop
/// reads the KEY's due, not the value). Stateless: every round rescans
/// from the head; crash recovery is the row's two stable states (row
/// present = not delivered, entry written = done).
pub fn sweep_due(shared: &state::Shared, now: u64) -> (usize, usize) {
    let mut due: Vec<DueRow> = Vec::new();
    let mut corrupt = 0usize;
    for slot in 0u32..=16383 {
        let prefix = crate::store::rocksdb::slot_prefix(slot as u16);
        let (lower, upper) = codec::delay_window(&prefix);
        // Early-stopping prefix scan of THIS slot's 0x1D window only:
        // the first key with due > now ends the walk (keys are
        // due-ordered); nothing outside the window is ever touched.
        let res = ops::for_each_from(&shared.store, &lower, false, &mut |k, v| {
            if k >= upper.as_slice() {
                return false; // left the 0x1D window
            }
            if due.len() >= ROUND_BUDGET {
                return false; // this round is full: rest rides the next
            }
            match codec::decode_delay_row_key(k, prefix.len()) {
                Some((due_ms, stream, _)) if due_ms <= now => {
                    match model::decode_entry(v) {
                        Some(fields) => due.push(DueRow {
                            key: k.to_vec(),
                            stream,
                            fields,
                        }),
                        // Undecodable body: the message is unrecoverable
                        // and the row would poison every later round.
                        None => corrupt += 1,
                    }
                    true
                }
                Some(_) => false, // first not-yet-due key: window done
                None => true,     // foreign byte layout: skip, keep walking
            }
        });
        if res.is_err() || due.len() >= ROUND_BUDGET {
            break; // store error: retry the whole round next tick
        }
    }
    if due.is_empty() {
        return (0, corrupt);
    }
    // Group by target stream preserving scan (due) order per stream.
    let mut groups: Vec<(Vec<u8>, Vec<&DueRow>)> = Vec::new();
    for row in &due {
        match groups.iter_mut().find(|(s, _)| *s == row.stream) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((row.stream.clone(), vec![row])),
        }
    }
    let mut totals = (0usize, corrupt);
    for (stream, rows) in groups {
        let (x, d) = exchange_stream(shared, now, &stream, &rows);
        totals.0 += x;
        totals.1 += d;
    }
    totals
}

/// Exchange one stream's due rows: try the stream latch (a mid-command
/// stream contributes nothing this round -- never parked on), re-read
/// the meta under it, then ONE batch = row deletes + entry writes +
/// meta maintenance (len +n, last_id = last fresh id, idle retouch).
/// A missing/purged meta means the family died with rows still staged
/// (a rename or family-delete race): the rows are deleted, never
/// exchanged -- a deleted stream must not be revived by its leftovers.
fn exchange_stream(
    shared: &state::Shared,
    now: u64,
    stream: &[u8],
    rows: &[&DueRow],
) -> (usize, usize) {
    let Some(prefix) = model::stream_prefix(stream) else {
        // No parent part (renamed to a slash-less name): no slot layout,
        // no meta key -- the row can never exchange. Drop it.
        return drop_rows(shared, rows);
    };
    let mkey = model::meta_key(&prefix, stream);
    let Some(_guard) = latch::try_lock(&shared.latch, &mkey) else {
        return (0, 0); // busy: retry next round
    };
    let meta = model::read_meta(&shared.store, &prefix, stream, Some(shared.lite.as_ref()));
    let Ok(MetaRead::Live(meta)) = meta else {
        return drop_rows(shared, rows); // Purged already folded the rows
    };
    let old_expire = model::current_expire(&shared.store, &prefix, stream);
    // Fresh ids in due order (see the module doc's ID POLICY).
    let mut last = meta.last_id();
    let mut batch = WriteBatch::default();
    let mut exchanged = 0usize;
    for row in rows {
        let Some(id) = model::auto_id(Some(last), now) else {
            break; // id ceiling: the rest stays staged, retried later
        };
        last = id;
        let fpairs: Vec<(&[u8], &[u8])> = row
            .fields
            .iter()
            .map(|(f, v)| (f.as_slice(), v.as_slice()))
            .collect();
        batch.put(
            model::entry_key(&prefix, stream, id),
            model::encode_entry(&fpairs),
        );
        batch.delete(row.key.clone());
        exchanged += 1;
    }
    if exchanged == 0 {
        return (0, 0); // nothing allocatable: next round retries
    }
    let mut next = meta.clone();
    next.last_ms = last.ms;
    next.last_seq = last.seq;
    next.len += exchanged as u64;
    // Idle retouch, mirroring XADD (an exchange is a write: idle = no
    // writes must not reap a stream whose only traffic is delayed).
    let new_expire = if next.idle_ms == 0 {
        0
    } else {
        match now.max(last.ms).checked_add(next.idle_ms) {
            Some(deadline) => deadline,
            None => return (0, 0), // refuse rather than wrap the deadline
        }
    };
    batch.put(&mkey, model::encode_meta_at(&next, new_expire));
    expire::set_ttl_entries(&mut batch, &prefix, mkey.clone(), old_expire, new_expire);
    if ops::batch_write(&shared.store, batch).is_err() {
        return (0, 0); // nothing committed: rows stay staged
    }
    // Wake every hub key a blocked reader could be parked on (XADD
    // parity: the stream's meta key and the bare parent topic's).
    let parent = &stream[..stream.iter().position(|&b| b == b'/').unwrap_or(0)];
    wait::notify(&shared.wait_hub, &mkey);
    wait::notify(&shared.wait_hub, &model::meta_key(&prefix, parent));
    (exchanged, 0)
}

/// Delete staged rows without exchanging them (orphans); returns
/// `(0, dropped)`. A failed write leaves the rows for the next round.
fn drop_rows(shared: &state::Shared, rows: &[&DueRow]) -> (usize, usize) {
    let mut batch = WriteBatch::default();
    for row in rows {
        batch.delete(row.key.clone());
    }
    match ops::batch_write(&shared.store, batch) {
        Ok(()) => (0, rows.len()),
        Err(_) => (0, 0),
    }
}

/// Background due-sweep loop (the `redeliver_loop` pattern): one
/// [`sweep_due`] round every `lite.delay_sweep_ms` on the blocking
/// pool; not spawned at all when the knob is 0 (default, OFF).
pub fn spawn_delay_sweep(shared: Arc<state::Shared>) {
    let period = shared.conf.lite.delay_sweep_ms;
    if period == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(period));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            // Sync scans + one synced write per victim stream: blocking
            // pool, never a tokio worker. The sweep owns no state, so a
            // panicked round costs nothing but this tick.
            let sh = Arc::clone(&shared);
            let _ = tokio::task::spawn_blocking(move || sweep_due(&sh, expire::now_ms())).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_row_key_orders_by_due_and_roundtrips() {
        let p = b"42/";
        let early = codec::delay_row_key(p, 100, b"a/q", (1, 1));
        let late = codec::delay_row_key(p, 200, b"a/q", (0, 0));
        assert!(early < late, "due must dominate the key order");
        assert_eq!(
            codec::decode_delay_row_key(&early, p.len()),
            Some((100, b"a/q".to_vec(), (1, 1)))
        );
        assert_eq!(codec::decode_delay_row_key(&late, 2), None);
    }
}
