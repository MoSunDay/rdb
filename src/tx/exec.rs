//! The EXEC engine: latch, validate WATCHes, replay, frame the array.
//!
//! Ordering (each step only after the previous one's guarantees hold):
//! 1. DIRTY check -- a queue-time rejection aborts wholesale (EXECABORT);
//!    nothing was ever executed, no latches needed.
//! 2. Latch every physical root of every queued key, byte-sorted
//!    (deadlock rule), and hold ALL of them across the whole replay:
//!    that is the transaction's isolation.
//! 3. WATCH validation UNDER the latches: re-hash each watched key and
//!    compare; any change (including lazy expires performed by other
//!    connections' reads) aborts with a null array. Doing this under the
//!    latches closes the check-then-act race.
//! 4. Replay each queued command through the SAME dispatch path used
//!    outside transactions; the task-local latch scope makes the
//!    handlers' own latch acquisitions reentrant no-ops, and their
//!    `Ctx::commit` calls land while the latches are still held.
//! 5. Splice an `*N` header before the buffered replies, release the
//!    latches, clear watches (EXEC always unwatches).

use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

use crate::command;
use crate::command::keys_core::latch_key;
use crate::ds::latch;
use crate::resp::codec;
use crate::state::Shared;
use crate::tx::session::ConnState;
use crate::tx::watch;

/// Redis EXECABORT reply for dirty transactions.
pub const EXECABORT: &str = "EXECABORT Transaction discarded because of previous errors.";

/// Run EXEC. The caller (tx_cmd) has already verified MULTI is open and
/// took nothing; this consumes the MULTI state either way.
pub async fn run(shared: &Shared, conn: &mut ConnState, out: &mut Vec<u8>, close: &mut bool) {
    let Some(multi) = conn.multi.take() else {
        // unreachable: the handler checks; keep the guard for safety
        codec::append_error(out, "ERR EXEC without MULTI");
        return;
    };
    let start = Instant::now();

    if multi.dirty {
        codec::append_error(out, EXECABORT);
        conn.clear_watches();
        crate::monitor::tx_event(&shared.monitor, "aborts");
        return;
    }
    if multi.queued.is_empty() {
        codec::append_array(out, 0);
        conn.clear_watches();
        return;
    }

    // 2. latch every physical root, byte-sorted.
    let prefix = slot_prefix(multi.slot);
    let latch_keys: BTreeSet<Vec<u8>> = multi
        .queued_keys()
        .into_iter()
        .map(|k| latch_key(&prefix, &k))
        .collect();
    let mut guards = Vec::with_capacity(latch_keys.len());
    for key in &latch_keys {
        guards.push(latch::lock(&shared.latch, key).await);
    }

    // 3. WATCH validation under the latches.
    if !conn.watches.is_empty() {
        let conflicted = conn
            .watches
            .iter()
            .any(|w| watch::value_hash(&shared.store, &w.prefix, &w.key) != w.hash);
        if conflicted {
            out.extend_from_slice(b"*-1\r\n"); // null array: aborted
            conn.clear_watches();
            drop(guards);
            crate::monitor::tx_event(&shared.monitor, "conflicts");
            return;
        }
    }

    // 4. replay through the standard dispatch path.
    let count = multi.queued.len();
    let header_at = out.len();
    let held: HashSet<Vec<u8>> = latch_keys.into_iter().collect();
    latch::exec_begin(held);
    for argv in multi.queued {
        command::dispatch(shared, argv, conn, out, close).await;
    }
    latch::exec_end();

    // 5. frame the buffered replies as one array and finish.
    let header = format!("*{count}\r\n");
    out.splice(header_at..header_at, header.into_bytes());
    conn.clear_watches();
    drop(guards);
    crate::monitor::tx_event(&shared.monitor, "commits");
    crate::monitor::tx_commit_latency(&shared.monitor, start.elapsed().as_millis() as f64);
}

/// Physical prefix for a slot ("<decimal>/"): same bytes routing uses.
fn slot_prefix(slot: u16) -> Vec<u8> {
    let mut prefix = slot.to_string().into_bytes();
    prefix.push(b'/');
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_prefix_is_decimal_slash() {
        assert_eq!(slot_prefix(0), b"0/".to_vec());
        assert_eq!(slot_prefix(42), b"42/".to_vec());
        assert_eq!(slot_prefix(16383), b"16383/".to_vec());
    }
}
