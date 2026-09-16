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

use crate::sql::columnar;
use crate::sql::storage::row;
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

/// Boot-time fallback for the one store state [`recover`] cannot answer:
/// the key is MISSING because the store was written by a pre-floor
/// binary and upgraded in place. A plain `recover` would report 0, the
/// oracle would restart at 1, and every persisted row version would
/// shade its own rewrite -- the exact defect the floor key exists to
/// fence. This scan walks the WHOLE keyspace once and returns the
/// highest ts any persisted MVCC record carries:
///
/// - SQL row versions (`<slot>/0x20<table><pk><ts>`): key-only parse
///   via [`row::parse_version_key`]; tombstone and prepared 2PC
///   versions share that key layout, so all of them count. A crc16
///   cross-check (`slot == slot_of(table_id, pk)`, true for every
///   writer) keeps out RESP user keys that merely LOOK like a row
///   version after their `"N/"` slot prefix; a slipped-through mimic
///   could only push the floor UP, which is safe (ts values are not
///   dense), but the check keeps that honest instead of counting every
///   space-led (0x20) string key.
/// - Columnar segment metas (kind 0x23): segment visibility rides the
///   SAME local oracle (`commit_ts <= read_ts`, see `columnar/reader`),
///   so they must count; their ts lives in the JSON VALUE, not the key,
///   so the scan reads those values. Metas are one tiny blob per
///   immutable segment, so this is cheap even on wide tables.
///
/// Index keys (0x21/0x22) carry no ts of their own (visibility is
/// re-derived from the row versions they point at), the catalog lives
/// in the raft store, and the floor key itself (`0x00` lead, no `/`)
/// parses as nothing -- none of them contribute.
///
/// Cost: one full keyspace walk, keys only (plus the few 0x23 values).
/// It runs ONLY when the floor key is absent -- a fresh store (empty,
/// returns instantly) or the FIRST boot after an in-place binary
/// upgrade; the boot path then stamps the key, so no later boot scans.
/// That one-shot price is the local-store twin of the co-upgrade
/// window COMPAT.md already requires for catalog-shape changes: a
/// pre-floor binary's data is recovered by the first post-floor boot.
/// Pure read: the store is never modified. A mid-scan iterator error
/// keeps the partial max (still a floor above 0, never below anything
/// observed) and logs loudly.
pub fn scan_max_ts(store: &Store) -> u64 {
    let mut max = 0u64;
    let scan = ops::for_each_from(store, b"", false, &mut |key, value| {
        if let Some((slot, table_id, pk, ts)) = row::parse_version_key(key) {
            if row::slot_of(table_id, &pk) == slot && ts > max {
                max = ts;
            }
        } else if columnar::meta::parse_meta_key(key).is_some() {
            if let Ok(m) = columnar::meta::decode_meta(value) {
                max = max.max(m.commit_ts);
            }
        }
        true
    });
    if let Err(e) = scan {
        eprintln!("sql ts: floor boot scan stopped early at {max}: {e}");
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::columnar::meta::{self, SegmentMeta, SegmentState};
    use crate::sql::storage::codec::KIND_SQL_ROW;
    use crate::sql::storage::row;
    use crate::store::rocksdb::slot_prefix;

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

    /// Canonical row-version key for `(table_id, pk, ts)` -- the same
    /// shape every writer produces (`slot_of` + kind + table + pk +
    /// inverted ts).
    fn vkey(table_id: u32, pk: &[u8], ts: u64) -> Vec<u8> {
        let mut k = slot_prefix(row::slot_of(table_id, pk));
        k.push(KIND_SQL_ROW);
        k.extend_from_slice(&table_id.to_be_bytes());
        k.extend_from_slice(pk);
        k.extend_from_slice(&row::ts_suffix(ts));
        k
    }

    fn segment_meta(commit_ts: u64) -> SegmentMeta {
        SegmentMeta {
            table_id: 9,
            table_name: "seg_table".to_string(),
            segment_id: commit_ts,
            commit_ts,
            state: SegmentState::Live,
            num_rows: 1,
            file: "seg.bin".to_string(),
            columns: vec![],
        }
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

    #[test]
    fn scan_max_ts_on_an_empty_store_is_zero() {
        let (dir, st) = store();
        assert_eq!(scan_max_ts(&st), 0);
        drop(st);
        drop(dir);
    }

    /// The floor key itself must never be mistaken for a row version or
    /// a segment meta: its 0x00 lead puts it before every slot band and
    /// kind byte, and it carries no `/` at all.
    #[test]
    fn floor_key_parses_as_neither_row_version_nor_segment_meta() {
        assert!(row::parse_version_key(FLOOR_KEY).is_none());
        assert!(meta::parse_meta_key(FLOOR_KEY).is_none());
    }

    /// The scan counts ONLY true row versions (live, tombstone and
    /// prepared share the key layout) and columnar segment metas, and
    /// takes their max. RESP keys, index keys (0x21), a RESP key that
    /// merely mimics the row-version shape under a WRONG slot, a
    /// malformed segment meta and the floor key itself are all ignored.
    #[test]
    fn scan_max_ts_counts_only_row_versions_and_segment_metas() {
        let (dir, st) = store();
        let mut batch = WriteBatch::default();

        // Row versions across two tables (two slots by construction):
        // live ts 10, tombstone ts 40, prepared 2PC ts 25.
        batch.put(vkey(7, b"a", 10), row::encode_tombstone());
        batch.put(vkey(9, b"c", 25), vec![row::HEADER_PREPARED]);
        batch.put(vkey(7, b"b", 40), vec![row::HEADER_LIVE]);
        // Plain RESP key: slot band, no SQL kind byte.
        let mut resp = slot_prefix(1);
        resp.extend_from_slice(b"plain:resp:key");
        batch.put(resp, b"v");
        // Secondary-index key (kind 0x21): no ts of its own, must not
        // count even when its bytes end in an inverted-ts shape.
        let mut idx = slot_prefix(2);
        idx.push(0x21);
        idx.extend_from_slice(&9u32.to_be_bytes());
        idx.extend_from_slice(&row::ts_suffix(8888));
        batch.put(idx, b"");
        // RESP key that MIMICS a row-version key (0x20 lead, table id,
        // pk, inverted ts) under a slot that is not slot_of(table, pk):
        // excluded by the crc16 cross-check.
        let mimic_table = 777u32;
        let mimic_pk = b"mimic";
        let true_slot = row::slot_of(mimic_table, mimic_pk);
        let wrong_slot = (true_slot + 7) % 16384;
        assert_ne!(wrong_slot, true_slot);
        let mut mimic = slot_prefix(wrong_slot);
        mimic.push(KIND_SQL_ROW);
        mimic.extend_from_slice(&mimic_table.to_be_bytes());
        mimic.extend_from_slice(mimic_pk);
        mimic.extend_from_slice(&row::ts_suffix(9999));
        batch.put(mimic, b"resp value");
        // Columnar segment metas: live meta at ts 55 (the overall max),
        // an undecodable one that must be skipped without panicking.
        batch.put(
            meta::meta_key(9, 55),
            meta::encode_meta(&segment_meta(55)).expect("encode meta"),
        );
        batch.put(meta::meta_key(9, 56), b"not json");
        // The floor key itself, stamped high: must not contribute.
        stamp(&mut batch, 777);

        ops::batch_write(&st, batch).expect("write");
        assert_eq!(
            scan_max_ts(&st),
            55,
            "max must be the segment meta's 55, not the mimic 9999, \
             the index 8888 or the floor stamp 777"
        );
        // Pure read: a second scan over the same store is unchanged.
        assert_eq!(scan_max_ts(&st), 55);
        drop(st);
        drop(dir);
    }
}
