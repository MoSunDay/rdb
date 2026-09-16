//! Persisted timestamp floor for the SQL MVCC oracle.
//!
//! Row versions are visible as `commit_ts <= read_ts`, so the oracle's
//! clock must NEVER run backwards across a restart: a fresh boot that
//! hands out ts 1 again makes every previously committed row invisible
//! (read snapshots sit below their commit ts) and rewrites of the same
//! pk get shadowed by the old, higher-ts versions.
//!
//! Cluster mode already fences the clock with the raft-replicated
//! `sql_ts_cursor` (see [`super::global`]); single-machine mode had no
//! equivalent. This module is that equivalent: every durable batch that
//! stamps MVCC records also stamps ONE store-reserved floor key, so the
//! highest timestamp that ever reached the store is itself durable (it
//! rides the batch's existing fsync -- no extra write). Boot replays
//! the key into the oracle (`Oracle::advance_to`, a max on both the
//! local counter and the cluster core). The floor only ever needs to be
//! at least the ts of any persisted record: it is safe for it to run
//! AHEAD (timestamps are not dense), so stamping the batch's max ts is
//! enough.

use rocksdb::WriteBatch;

use crate::store::ops;
use crate::store::Store;

/// Store-reserved physical key holding the floor as 8 big-endian bytes.
///
/// The leading 0x00 sorts below every `"N/"` slot band (ASCII digits)
/// and below every SQL kind byte (0x20..0x23), so it can never collide
/// with -- or be swept by -- data-path scans, which all start at a slot
/// or kind prefix.
pub const FLOOR_KEY: &[u8] = b"\x00sql_ts_floor";

/// Stamp the floor into a batch that carries ts-stamped MVCC records.
///
/// `ts` must be the HIGHEST timestamp any record in this batch carries
/// (callers pass their allocation range end - 1). Same-batch idempotent
/// overwrite: when one batch stamps several ranges (rows + columnar
/// segment metas), the LAST stamp wins, so callers re-stamp with the
/// overall max after appending the tail.
pub fn stamp(batch: &mut WriteBatch, ts: u64) {
    batch.put(FLOOR_KEY, ts.to_be_bytes());
}

/// Highest persisted floor on this store, 0 when the key is absent
/// (fresh store) or unreadable -- the pre-fix behavior, never a safety
/// regression (an under-reported floor only risks the old bug, it can
/// never shadow live data by itself).
pub fn recover(store: &Store) -> u64 {
    match ops::get_physical(store, FLOOR_KEY) {
        Ok(Some(raw)) if raw.len() == 8 => {
            u64::from_be_bytes(raw[..8].try_into().expect("8-byte floor"))
        }
        Ok(_) => 0,
        Err(e) => {
            eprintln!("sql ts: floor key unreadable, starting at 1: {e}");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let st = crate::store::open(
            crate::store::data_path(dir.path().to_str().unwrap(), "t")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("open store");
        (dir, st)
    }

    #[test]
    fn recover_defaults_to_zero_on_a_fresh_store() {
        let (dir, st) = store();
        assert_eq!(recover(&st), 0);
        drop(st);
        drop(dir);
    }

    #[test]
    fn stamp_and_recover_round_trip() {
        let (dir, st) = store();
        let mut batch = WriteBatch::default();
        stamp(&mut batch, 41);
        ops::batch_write(&st, batch).expect("write");
        assert_eq!(recover(&st), 41);
        drop(st);
        drop(dir);
    }

    #[test]
    fn stamp_is_an_idempotent_overwrite_and_last_write_wins() {
        let (dir, st) = store();
        let mut batch = WriteBatch::default();
        stamp(&mut batch, 9);
        stamp(&mut batch, 7); // re-stamp inside one batch: last one wins
        ops::batch_write(&st, batch).expect("write");
        assert_eq!(recover(&st), 7);
        // Monotonicity across batches is the CALLER's job (pass the
        // batch max); every stamp site passes its allocation end - 1.
        drop(st);
        drop(dir);
    }

    #[test]
    fn malformed_floor_value_reads_as_zero() {
        let (dir, st) = store();
        let mut batch = WriteBatch::default();
        batch.put(FLOOR_KEY, b"junk");
        ops::batch_write(&st, batch).expect("write");
        assert_eq!(recover(&st), 0);
        drop(st);
        drop(dir);
    }
}
