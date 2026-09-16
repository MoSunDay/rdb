//! Watermark-driven MVCC version garbage collection (M2).
//!
//! Row versions at or below the snapshot oracle's watermark are invisible
//! to every live snapshot: a reader picks, per primary key, the NEWEST
//! version with `ts <= read_ts`, and no snapshot reads below the
//! watermark. Within one pk group -- the versions of one
//! `(slot, table_id, pk_key)`, contiguous in the key and newest-first
//! (inverted ts suffix) -- the sweep applies:
//!
//! - versions with `ts > watermark` are never touched (snapshots at or
//!   beyond the watermark may still read them);
//! - the NEWEST version with `ts <= watermark` is the "anchor" -- the
//!   version a snapshot sitting exactly at the watermark resolves to. It
//!   is KEPT while live; a tombstone anchor is deleted too (all of its
//!   older siblings are garbage, so nothing can resurrect);
//! - every older version with `ts <= watermark` is shadowed by the
//!   anchor for every possible reader -> deleted.
//!
//! One sweep deletes at most [`MAX_DELETES_PER_SWEEP`] versions (bounded
//! work); [`run_gc_loop`] re-runs every ~30s and catches up. Deletes go
//! out in one synced `WriteBatch`, mirroring the expire sampler.
//!
//! DROPPED TABLES: a DROP TABLE leaves its row versions and index
//! entries physically behind (the catalog tombstone only makes them
//! unreachable; ids are never reused, so nothing aliases them). Every
//! node's background sweep also consults the replicated catalog's
//! dropped-id set: all versions of a dropped table's pk groups are
//! garbage regardless of the watermark (there are no readers by
//! definition), and its 0x21/0x22 index entries are deleted as they are
//! met. Columnar segments have their own sweep ([`crate::sql::columnar`]
//! M5) with the same catalog-driven rule.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use rocksdb::WriteBatch;

use crate::sql::index::keys;
use crate::sql::storage::catalog;
use crate::sql::storage::row::{parse_version_key, HEADER_TOMBSTONE};
use crate::state::Shared;
use crate::store::ops;
use crate::store::Store;

/// Upper bound on versions deleted by one sweep: keeps a single call
/// bounded; the periodic loop catches up on later rounds.
pub const MAX_DELETES_PER_SWEEP: usize = 10_000;

/// Background sweep period.
const GC_PERIOD: Duration = Duration::from_secs(30);

/// Streaming fold state over the newest-first version stream.
#[derive(Default)]
struct GroupCursor {
    /// `(slot, table_id, pk_key)` of the group currently being folded.
    group: Option<(u16, u32, Vec<u8>)>,
    /// Whether the anchor (newest version with `ts <= wm`) of the
    /// current group has been seen yet.
    anchor_seen: bool,
    /// True while the current group belongs to a dropped table: every
    /// version of the group is unreachable and is deleted (the dropped
    /// id set may grow between rounds, so this is re-derived on every
    /// group transition).
    dropped_group: bool,
}

/// Fold one parsed version into the sweep; `Some(key)` = delete it.
///
/// The physical walk delivers each pk group newest-first, so the FIRST
/// version with `ts <= wm` is the anchor (what a snapshot at the
/// watermark reads). Versions above `wm` never delete; older siblings
/// below the anchor always delete; a tombstone anchor takes itself out.
fn fold_version(
    cur: &mut GroupCursor,
    wm: u64,
    dropped: &HashSet<u32>,
    key: &[u8],
    parsed: (u16, u32, Vec<u8>, u64),
    val: &[u8],
) -> Option<Vec<u8>> {
    let (slot, table_id, pk, ts) = parsed;
    let same_group = cur
        .group
        .as_ref()
        .is_some_and(|(s, t, p)| *s == slot && *t == table_id && *p == pk);
    if !same_group {
        cur.group = Some((slot, table_id, pk));
        cur.anchor_seen = false;
        cur.dropped_group = dropped.contains(&table_id);
    }
    if cur.dropped_group {
        // Dropped table: no snapshot can ever read these versions, so
        // even the newest and prepared ones are garbage.
        return Some(key.to_vec());
    }
    if ts > wm {
        return None;
    }
    if cur.anchor_seen {
        return Some(key.to_vec()); // older sibling below the anchor
    }
    cur.anchor_seen = true;
    match val.first() {
        Some(&HEADER_TOMBSTONE) => Some(key.to_vec()),
        // Live anchor stays; unknown headers (M3 prepared 0x02) stay too.
        _ => None,
    }
}

/// Exclusive end of the slot keyspace: SQL row versions live under
/// `"<slot>/" + 0x20 + ...`, slot prefixes are decimal ASCII, so the
/// successor of the lexicographically largest prefix ("9999/" beats
/// every 5-digit "1xxxx/") bounds the whole region. Derived from the
/// real slot space, not a magic literal.
fn slot_space_end() -> Vec<u8> {
    let last = (0..crate::topology::SLOT_NUMBER as u16)
        .map(crate::store::rocksdb::slot_prefix)
        .max()
        .unwrap_or_default();
    crate::store::rocksdb::key_upper_bound(&last).unwrap_or_default()
}

/// One GC pass over the SQL row version space: delete every version made
/// invisible by `watermark`, plus everything left by tables in
/// `dropped` (dropped-table rows and 0x21/0x22 index entries). Returns
/// the number of versions deleted; scan/write failures log and delete
/// nothing.
pub fn sweep(store: &Store, watermark: u64, dropped: &HashSet<u32>) -> usize {
    sweep_capped(store, watermark, dropped, MAX_DELETES_PER_SWEEP)
}

/// Testable core of [`sweep`] with an explicit delete cap.
pub fn sweep_capped(store: &Store, watermark: u64, dropped: &HashSet<u32>, cap: usize) -> usize {
    // Same walk as exec::scan::visible_rows: forward from "0/" over the
    // whole slot space, filtering through parse_version_key. SQL row
    // versions INTERLEAVE with the RESP families inside every slot
    // region (RESP kinds end at 0x12 < 0x20 < 0x21 index kinds), so the
    // kind byte alone cannot bound the walk; non-SQL keys are skipped.
    let end = slot_space_end();
    let bounded = !end.is_empty();
    let mut cur = GroupCursor::default();
    let mut dead: Vec<Vec<u8>> = Vec::new();
    let scanned = ops::for_each_from(store, b"0/", false, &mut |key, val| {
        if bounded && key >= end.as_slice() {
            return false; // past the last slot: no SQL rows beyond
        }
        let Some(parsed) = parse_version_key(key) else {
            // 0x21/0x22 index entries of dropped tables are unreachable
            // garbage; delete them without touching the version cursor
            // (index keys can never split a pk version group: within a
            // slot+table every row key sorts before all index keys).
            if let Some((_, tid, _, _)) = keys::parse_index_key(key) {
                if dropped.contains(&tid) {
                    dead.push(key.to_vec());
                    if dead.len() >= cap {
                        return false;
                    }
                }
            }
            return true; // RESP / index key riding the same slots
        };
        if let Some(k) = fold_version(&mut cur, watermark, dropped, key, parsed, val) {
            dead.push(k);
            if dead.len() >= cap {
                return false; // capped: next sweep resumes from "0/"
            }
        }
        true
    });
    if let Err(e) = scanned {
        eprintln!("[sql-gc] version scan failed: {e}");
        return 0;
    }
    if dead.is_empty() {
        return 0;
    }
    let mut batch = WriteBatch::default();
    for k in &dead {
        batch.delete(k);
    }
    match ops::batch_write(store, batch) {
        Ok(()) => dead.len(),
        Err(e) => {
            eprintln!("[sql-gc] version delete batch failed: {e}");
            0
        }
    }
}

/// Periodic watermark GC (spawned from main.rs next to the expire and
/// Lite loops). Each round parks the sync RocksDB sweep on tokio's
/// blocking pool; quiet rounds log nothing.
pub async fn run_gc_loop(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(GC_PERIOD);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // consume the immediate first tick
    loop {
        ticker.tick().await;
        let wm = shared.sql_ts.watermark();
        let gc_shared = Arc::clone(&shared);
        if let Err(e) = tokio::task::spawn_blocking(move || {
            let dropped: HashSet<u32> = catalog::dropped_ids(&gc_shared).into_iter().collect();
            sweep(&gc_shared.store, wm, &dropped)
        })
        .await
        {
            eprintln!("[sql-gc] sweep task failed: {e}");
        }
    }
}

/// main.rs hook: run [`run_gc_loop`] on the normal listener's engine (the
/// backup listener is read-only by design and spawns no data-plane tasks).
pub fn spawn_gc(shared: Arc<Shared>) {
    tokio::spawn(run_gc_loop(shared));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::storage::row::{
        encode_row, encode_tombstone, pk_encode, row_slot, version_key, visible_value,
    };
    use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel, SqlType, TableSchema, Value};
    use crate::state::testutil;

    fn schema(id: u32) -> TableSchema {
        TableSchema {
            id,
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    sql_type: SqlType::Int,
                    nullable: false,
                },
                ColumnDef {
                    name: "v".into(),
                    sql_type: SqlType::VarChar,
                    nullable: true,
                },
            ],
            pk: vec!["id".to_string()],
            auto_increment: None,
            engine: Engine::Row,
            indexes: vec![],
            key_model: KeyModel::MySql,
            distribution: None,
        }
    }

    fn shared() -> Shared {
        testutil::shared_with(testutil::test_config())
    }

    /// Write one version: `live = Some(text)` -> live row, `None` -> tombstone.
    fn put(shared: &Shared, s: &TableSchema, pk: i64, ts: u64, live: Option<&str>) {
        let pk_key = pk_encode(&Value::Int(pk)).unwrap();
        let key = version_key(s, row_slot(s, &pk_key), &pk_key, ts);
        let val = match live {
            Some(text) => encode_row(s, &[Value::Int(pk), Value::Str(text.into())]).unwrap(),
            None => encode_tombstone(),
        };
        let mut batch = WriteBatch::default();
        batch.put(key, val);
        ops::batch_write(&shared.store, batch).unwrap();
    }

    /// Remaining versions of one pk as `(ts, raw value)`, newest-first
    /// (the order the physical walk delivers them in).
    fn versions_of(shared: &Shared, s: &TableSchema, pk: i64) -> Vec<(u64, Vec<u8>)> {
        let pk_key = pk_encode(&Value::Int(pk)).unwrap();
        let mut out = Vec::new();
        ops::for_each_from(&shared.store, b"0/", false, &mut |key, val| {
            if let Some((_, tid, p, ts)) = parse_version_key(key) {
                if tid == s.id && p == pk_key {
                    out.push((ts, val.to_vec()));
                }
            }
            true
        })
        .unwrap();
        out
    }

    fn ts_of(versions: &[(u64, Vec<u8>)]) -> Vec<u64> {
        versions.iter().map(|(ts, _)| *ts).collect()
    }

    #[test]
    fn tombstone_anchor_deletes_whole_group() {
        let shared = shared();
        let s = schema(7);
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2"));
        put(&shared, &s, 1, 3, None);
        assert_eq!(sweep(&shared.store, 3, &HashSet::new()), 3);
        assert!(versions_of(&shared, &s, 1).is_empty());
    }

    #[test]
    fn live_anchor_kept_older_deleted_newer_untouched() {
        let shared = shared();
        let s = schema(7);
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2"));
        put(&shared, &s, 1, 3, None);
        assert_eq!(sweep(&shared.store, 2, &HashSet::new()), 1);
        assert_eq!(ts_of(&versions_of(&shared, &s, 1)), vec![3, 2]);
        // A reader at read_ts=2 still resolves to the kept ts2 row.
        let expected = encode_row(&s, &[Value::Int(1), Value::Str("v2".into())]).unwrap();
        let left = versions_of(&shared, &s, 1);
        let (val, ts) = visible_value(left.iter().map(|(t, v)| (*t, v.as_slice())), 2).unwrap();
        assert_eq!(ts, 2);
        assert_eq!(val, expected.as_slice());
    }

    #[test]
    fn zero_watermark_deletes_nothing() {
        let shared = shared();
        let s = schema(7);
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2"));
        assert_eq!(sweep(&shared.store, 0, &HashSet::new()), 0);
        assert_eq!(ts_of(&versions_of(&shared, &s, 1)), vec![2, 1]);
    }

    #[test]
    fn versions_above_watermark_survive() {
        let shared = shared();
        let s = schema(7);
        for ts in 1..=4u64 {
            put(&shared, &s, 1, ts, Some("v"));
        }
        assert_eq!(sweep(&shared.store, 2, &HashSet::new()), 1); // only ts1 (below the anchor)
        assert_eq!(ts_of(&versions_of(&shared, &s, 1)), vec![4, 3, 2]);
    }

    #[test]
    fn cap_bounds_one_sweep() {
        let shared = shared();
        let s = schema(7);
        for ts in 1..=4u64 {
            put(&shared, &s, 1, ts, Some("v"));
        }
        // anchor ts4 kept; ts3+ts2 fill the cap; ts1 waits for the next sweep.
        assert_eq!(sweep_capped(&shared.store, 4, &HashSet::new(), 2), 2);
        assert_eq!(ts_of(&versions_of(&shared, &s, 1)), vec![4, 1]);
        assert_eq!(sweep_capped(&shared.store, 4, &HashSet::new(), 2), 1);
        assert_eq!(ts_of(&versions_of(&shared, &s, 1)), vec![4]);
    }

    #[test]
    fn adjacent_pk_groups_fold_independently() {
        let shared = shared();
        let s = schema(7);
        // Insertion order interleaves; the physical walk groups by pk_key.
        put(&shared, &s, 1, 1, Some("a1"));
        put(&shared, &s, 2, 1, Some("b1"));
        put(&shared, &s, 1, 2, Some("a2"));
        put(&shared, &s, 2, 2, None);
        put(&shared, &s, 1, 3, None);
        put(&shared, &s, 2, 3, Some("b3"));
        // wm=3: pk1's tombstone anchor wipes its group (3 deletes);
        // pk2 keeps its live anchor (2 deletes).
        assert_eq!(sweep(&shared.store, 3, &HashSet::new()), 5);
        assert!(versions_of(&shared, &s, 1).is_empty());
        assert_eq!(ts_of(&versions_of(&shared, &s, 2)), vec![3]);
    }

    #[test]
    fn same_pk_different_tables_are_separate_groups() {
        let shared = shared();
        let a = schema(7);
        let b = schema(9);
        put(&shared, &a, 1, 1, Some("a1"));
        put(&shared, &a, 1, 2, Some("a2"));
        put(&shared, &b, 1, 1, Some("b1"));
        put(&shared, &b, 1, 2, None);
        // wm=2: table a keeps its live anchor (ts1 dies); table b's
        // tombstone anchor takes the whole group out.
        assert_eq!(sweep(&shared.store, 2, &HashSet::new()), 3);
        assert_eq!(ts_of(&versions_of(&shared, &a, 1)), vec![2]);
        assert!(versions_of(&shared, &b, 1).is_empty());
    }

    #[test]
    fn oldest_registered_snapshot_pins_its_anchor() {
        let shared = shared();
        let s = schema(7);
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2"));
        put(&shared, &s, 1, 5, Some("v5"));
        shared.sql_ts.register_snapshot(2); // BEGIN at read_ts=2
        assert_eq!(
            sweep(&shared.store, shared.sql_ts.watermark(), &HashSet::new()),
            1
        );
        let left = versions_of(&shared, &s, 1);
        assert_eq!(ts_of(&left), vec![5, 2]);
        let (val, ts) = visible_value(left.iter().map(|(t, v)| (*t, v.as_slice())), 2).unwrap();
        assert_eq!(ts, 2);
        assert_eq!(val.first(), Some(&crate::sql::storage::row::HEADER_LIVE));
        shared.sql_ts.unregister_snapshot(2);
    }

    #[test]
    fn resp_keys_sharing_the_slot_are_untouched() {
        let shared = shared();
        let s = schema(7);
        // RESP-plane key in the SAME slot, kind byte < 0x20: sorts ahead
        // of the SQL rows, must be skipped (not used as a region bound).
        let pk_key = pk_encode(&Value::Int(1)).unwrap();
        let mut resp_key = crate::store::rocksdb::slot_prefix(row_slot(&s, &pk_key));
        resp_key.push(0x01);
        resp_key.extend_from_slice(b"plain");
        let mut batch = WriteBatch::default();
        batch.put(resp_key.clone(), b"v");
        ops::batch_write(&shared.store, batch).unwrap();
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2"));
        assert_eq!(sweep(&shared.store, 2, &HashSet::new()), 1);
        assert_eq!(
            ops::get_physical(&shared.store, &resp_key).unwrap(),
            Some(b"v".to_vec())
        );
    }

    #[test]
    fn dropped_table_rows_deleted_beyond_watermark() {
        let shared = shared();
        let s = schema(7);
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2"));
        put(&shared, &s, 1, 3, None); // newest is a tombstone
        let dropped: HashSet<u32> = [7].into();
        // Watermark 2: the ts3 tombstone is above it, yet the whole
        // dropped group (every version, any header) must go.
        assert_eq!(sweep(&shared.store, 2, &dropped), 3);
        assert!(versions_of(&shared, &s, 1).is_empty());
    }

    #[test]
    fn dropped_table_rows_deleted_with_live_anchor() {
        let shared = shared();
        let s = schema(7);
        put(&shared, &s, 1, 1, Some("v1"));
        put(&shared, &s, 1, 2, Some("v2")); // would be the live anchor
        let dropped: HashSet<u32> = [7].into();
        assert_eq!(sweep(&shared.store, 2, &dropped), 2);
        assert!(versions_of(&shared, &s, 1).is_empty());
    }

    #[test]
    fn dropped_table_does_not_touch_other_tables() {
        let shared = shared();
        let a = schema(7);
        let b = schema(8);
        put(&shared, &a, 1, 1, Some("a1"));
        put(&shared, &b, 1, 1, Some("b1"));
        put(&shared, &b, 1, 2, Some("b2"));
        let dropped: HashSet<u32> = [7].into();
        // a's dropped version + b's shadowed ts1 version.
        assert_eq!(sweep(&shared.store, 2, &dropped), 2);
        assert!(versions_of(&shared, &a, 1).is_empty());
        assert_eq!(ts_of(&versions_of(&shared, &b, 1)), vec![2]);
    }

    #[test]
    fn dropped_table_index_entries_deleted() {
        use crate::sql::index::keys::{secondary_key, unique_key};
        let shared = shared();
        // One secondary and one unique entry under the dropped table.
        let col_key = crate::sql::storage::codec::encode_key(&Value::Str("x".into())).unwrap();
        let pk_key = pk_encode(&Value::Int(1)).unwrap();
        let mut batch = WriteBatch::default();
        batch.put(secondary_key(7, 0, &col_key, &pk_key), b"".as_slice());
        batch.put(unique_key(7, 1, &col_key), &pk_key);
        ops::batch_write(&shared.store, batch).unwrap();
        let dropped: HashSet<u32> = [7].into();
        assert_eq!(sweep(&shared.store, 1, &dropped), 2);
        let mut leftovers = 0usize;
        ops::for_each_from(&shared.store, b"0/", false, &mut |key, _| {
            if let Some((_, tid, _, _)) = keys::parse_index_key(key) {
                if tid == 7 {
                    leftovers += 1;
                }
            }
            true
        })
        .unwrap();
        assert_eq!(leftovers, 0);
    }
}
