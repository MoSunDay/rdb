//! Extra storage operations for the typed data-structure phase: range
//! deletes, batched sync writes and bounded ordered scans (SCAN/KEYS and
//! the expire sampler build on these).
//!
//! Everything here is a free function over `&Store`, mirroring
//! `store::rocksdb`. Writes keep the `sync = true` discipline; the async
//! twins park the fsync on tokio's blocking pool like `set_async` does.

use std::sync::Arc;

use rocksdb::{Direction, IteratorMode, ReadOptions, WriteBatch};

use super::rocksdb::{sync_write_opts, Store};

/// Get by full physical key (callers that already composed prefix + body).
pub fn get_physical(store: &Store, physical: &[u8]) -> Result<Option<Vec<u8>>, String> {
    store.db.get(physical).map_err(|e| e.to_string())
}

/// Range delete `[lower, upper)` in one RocksDB delete_range tombstone.
/// An EMPTY `upper` means "to the end of the keyspace": delete_range
/// cannot express +inf, so that (rare -- all-0xff roots only) path falls
/// back to iterating and batch-deleting every key from `lower`.
pub fn delete_range(store: &Store, lower: &[u8], upper: &[u8]) -> Result<(), String> {
    if upper.is_empty() {
        let mut batch = WriteBatch::default();
        for_each_from(store, lower, false, &mut |k, _| {
            batch.delete(k);
            true
        })?;
        return batch_write(store, batch);
    }
    let mut batch = WriteBatch::default();
    batch.delete_range(lower, upper);
    batch_write(store, batch)
}

/// Commit a batch with the usual synchronous WriteOptions.
pub fn batch_write(store: &Store, batch: WriteBatch) -> Result<(), String> {
    store
        .db
        .write_opt(batch, &sync_write_opts())
        .map_err(|e| e.to_string())
}

/// Async twin of [`batch_write`]: the fsync runs off-worker.
pub async fn batch_write_async(store: Arc<Store>, batch: WriteBatch) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        store
            .db
            .write_opt(batch, &sync_write_opts())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Fire-and-forget twin of [`batch_write`]: commit off-worker and DROP
/// the result (failures only log). Lazy-expiry purges use this so a read
/// running on a tokio worker never performs an inline RocksDB fsync --
/// the purge DECISION is already made, so the caller does not wait for
/// the write to land.
pub fn spawn_batch_write(store: Arc<Store>, batch: WriteBatch) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = store.db.write_opt(batch, &sync_write_opts()) {
            eprintln!("rdb: detached batch write failed: {e}");
        }
    });
}

/// Async twin of [`delete_range`].
pub async fn delete_range_async(
    store: Arc<Store>,
    lower: Vec<u8>,
    upper: Vec<u8>,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || delete_range(&store, &lower, &upper))
        .await
        .map_err(|e| e.to_string())?
}

/// Keys per batch in [`delete_range_paged`]: bounds each synced write so
/// an empty-upper wipe cannot grow one unbounded fsync-ed WriteBatch.
const DELETE_PAGE: usize = 1000;

/// Paged deletion of `[lower, upper)` (EMPTY `upper` = to the end of the
/// keyspace). Unlike [`delete_range`]'s empty-upper path -- one batch
/// holding every key in the keyspace behind a single fsync -- each page
/// deletes at most [`DELETE_PAGE`] keys in its own synced batch and the
/// scan resumes where the last page stopped, until a page comes back
/// empty. Returns the number of keys deleted.
pub fn delete_range_paged(store: &Store, lower: &[u8], upper: &[u8]) -> Result<usize, String> {
    delete_range_paged_inner(store, lower, upper, DELETE_PAGE)
}

/// Async twin of [`delete_range_paged`]: every page's collect-and-fsync
/// runs on tokio's blocking pool, with a yield between pages so a long
/// wipe stays cooperative.
pub async fn delete_range_paged_async(
    store: Arc<Store>,
    lower: Vec<u8>,
    upper: Vec<u8>,
) -> Result<usize, String> {
    let mut cursor = lower;
    let mut total = 0usize;
    loop {
        let (deleted, next) = {
            let from = cursor.clone();
            let to = upper.clone();
            let shared = Arc::clone(&store);
            tokio::task::spawn_blocking(move || delete_page_once(&shared, &from, &to, DELETE_PAGE))
                .await
                .map_err(|e| e.to_string())??
        };
        tokio::task::yield_now().await;
        if deleted == 0 {
            return Ok(total);
        }
        total += deleted;
        cursor = next;
    }
}

/// Testable core of [`delete_range_paged`] with an explicit page size.
fn delete_range_paged_inner(
    store: &Store,
    lower: &[u8],
    upper: &[u8],
    page: usize,
) -> Result<usize, String> {
    let mut cursor = lower.to_vec();
    let mut total = 0usize;
    loop {
        let (deleted, next) = delete_page_once(store, &cursor, upper, page)?;
        if deleted == 0 {
            return Ok(total); // empty page: the range is exhausted
        }
        total += deleted;
        cursor = next;
    }
}

/// One page of a paged wipe: collect up to `page` keys in
/// `[cursor, upper)` (empty `upper` = no upper bound) and batch-delete
/// them in one synced write. Returns `(deleted, next_cursor)` where
/// `next_cursor` is the first key the page did NOT take -- the next page
/// scans inclusive from it; when the scan ran out (or left the window)
/// it stays at `cursor`, whose keys are gone, so the follow-up page
/// comes back empty. `(0, _)` means nothing left to delete.
fn delete_page_once(
    store: &Store,
    cursor: &[u8],
    upper: &[u8],
    page: usize,
) -> Result<(usize, Vec<u8>), String> {
    let page = page.max(1); // a 0 page would never collect anything
    let mut keys: Vec<Vec<u8>> = Vec::with_capacity(page);
    let mut next = cursor.to_vec();
    for_each_from(store, cursor, false, &mut |k, _| {
        if !upper.is_empty() && k >= upper {
            return false; // left the window
        }
        if keys.len() == page {
            next = k.to_vec(); // page full: resume at this unvisited key
            return false;
        }
        keys.push(k.to_vec());
        true
    })?;
    if keys.is_empty() {
        return Ok((0, next));
    }
    let mut batch = WriteBatch::default();
    for k in &keys {
        batch.delete(k);
    }
    batch_write(store, batch)?;
    Ok((keys.len(), next))
}

fn forward_iter<'a>(
    store: &'a Store,
    from: &[u8],
    upper: Option<Vec<u8>>,
) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a {
    let mut ropts = ReadOptions::default();
    ropts.set_iterate_lower_bound(from.to_vec());
    if let Some(upper) = upper {
        ropts.set_iterate_upper_bound(upper);
    }
    store
        .db
        .iterator_opt(IteratorMode::From(from, Direction::Forward), ropts)
        .map_while(Result::ok)
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
}

/// Ordered scan from `from`; the callback returns `false` to stop early.
/// `excl_from` skips a leading key equal to `from` (cursor resume).
/// Iteration errors abort the scan and surface as `Err` -- the raw
/// RocksDB iterator is consumed item-by-item (NOT via `forward_iter`,
/// whose `map_while(Result::ok)` would swallow an error into a silent
/// early stop and report `Ok`).
pub fn for_each_from(
    store: &Store,
    from: &[u8],
    excl_from: bool,
    f: &mut dyn FnMut(&[u8], &[u8]) -> bool,
) -> Result<(), String> {
    let mut ropts = ReadOptions::default();
    ropts.set_iterate_lower_bound(from.to_vec());
    let mut iter = store
        .db
        .iterator_opt(IteratorMode::From(from, Direction::Forward), ropts);
    let mut skip_leading = excl_from;
    for item in &mut iter {
        let (k, v) = item.map_err(|e| e.to_string())?;
        if skip_leading {
            skip_leading = false;
            if k.as_ref() == from {
                continue;
            }
        }
        if !f(&k, &v) {
            break;
        }
    }
    Ok(())
}

/// Owned `(key, value)` pair list, as returned by the collect helpers.
pub type KvPairs = Vec<(Vec<u8>, Vec<u8>)>;

/// Collect up to `limit` `(physical key, value)` pairs with `prefix`.
/// `limit == 0` means unbounded.
pub fn prefix_iter_collect(store: &Store, prefix: &[u8], limit: usize) -> Result<KvPairs, String> {
    let upper = super::rocksdb::key_upper_bound(prefix);
    let mut out = Vec::new();
    for (k, v) in forward_iter(store, prefix, upper) {
        out.push((k, v));
        if limit != 0 && out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// Collect up to `limit` physical keys with `prefix` (`0` = unbounded).
/// Matches `iter_prefix`'s swallow-errors contract (best effort).
pub fn prefix_keys_collect(store: &Store, prefix: &[u8], limit: usize) -> Vec<Vec<u8>> {
    let upper = super::rocksdb::key_upper_bound(prefix);
    let mut out = Vec::new();
    for (k, _) in forward_iter(store, prefix, upper) {
        out.push(k);
        if limit != 0 && out.len() >= limit {
            break;
        }
    }
    out
}

/// First physical key >= `from`, if any.
pub fn first_key_from(store: &Store, from: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut ropts = ReadOptions::default();
    ropts.set_iterate_lower_bound(from.to_vec());
    let mut it = store
        .db
        .iterator_opt(IteratorMode::From(from, Direction::Forward), ropts);
    Ok(it.next().and_then(Result::ok).map(|(k, _)| k.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::super::rocksdb;
    use super::*;

    fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = rocksdb::open(dir.path().to_str().unwrap()).expect("open");
        (dir, store)
    }

    #[test]
    fn delete_range_removes_only_window() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"70/", b"a", b"1").unwrap();
        rocksdb::set(&store, b"70/", b"b", b"2").unwrap();
        rocksdb::set(&store, b"70/", b"c", b"3").unwrap();
        delete_range(&store, b"70/b", b"70/c").unwrap();
        assert_eq!(rocksdb::get(&store, b"70/", b"a"), Ok(Some(b"1".to_vec())));
        assert_eq!(rocksdb::get(&store, b"70/", b"b"), Ok(None));
        assert_eq!(rocksdb::get(&store, b"70/", b"c"), Ok(Some(b"3".to_vec())));
    }

    #[test]
    fn delete_range_empty_upper_wipes_to_end() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"10/", b"low", b"0").unwrap();
        rocksdb::set(&store, b"70/", b"a", b"1").unwrap();
        rocksdb::set(&store, b"99/", b"z", b"9").unwrap();
        delete_range(&store, b"70/", b"").unwrap();
        assert_eq!(rocksdb::get(&store, b"70/", b"a"), Ok(None));
        assert_eq!(rocksdb::get(&store, b"99/", b"z"), Ok(None)); // >= 70/
        assert_eq!(
            rocksdb::get(&store, b"10/", b"low"),
            Ok(Some(b"0".to_vec()))
        ); // < 70/
    }

    #[test]
    fn batch_write_is_atomic_and_sync() {
        let (_dir, store) = open_tmp();
        let mut batch = WriteBatch::default();
        batch.put(b"70/x", b"1");
        batch.put(b"70/y", b"2");
        batch.delete(b"70/x");
        batch_write(&store, batch).unwrap();
        assert_eq!(rocksdb::get(&store, b"70/", b"x"), Ok(None));
        assert_eq!(rocksdb::get(&store, b"70/", b"y"), Ok(Some(b"2".to_vec())));
    }

    #[test]
    fn prefix_collect_respects_limit() {
        let (_dir, store) = open_tmp();
        for k in ["a", "b", "c"] {
            rocksdb::set(&store, b"70/", k.as_bytes(), b"v").unwrap();
        }
        rocksdb::set(&store, b"71/", b"other", b"v").unwrap();
        let pairs = prefix_iter_collect(&store, b"70/", 2).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, b"70/a".to_vec());
        let all = prefix_iter_collect(&store, b"70/", 0).unwrap();
        assert_eq!(all.len(), 3);
        let keys = prefix_keys_collect(&store, b"70/", 0);
        assert_eq!(
            keys,
            vec![b"70/a".to_vec(), b"70/b".to_vec(), b"70/c".to_vec()]
        );
        assert_eq!(prefix_keys_collect(&store, b"70/", 2).len(), 2);
        assert!(prefix_keys_collect(&store, b"88/", 0).is_empty());
    }

    #[test]
    fn for_each_from_visits_ordered_and_stops() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"70/", b"a", b"1").unwrap();
        rocksdb::set(&store, b"70/", b"b", b"2").unwrap();
        rocksdb::set(&store, b"70/", b"c", b"3").unwrap();
        let mut seen = Vec::new();
        for_each_from(&store, b"70/", false, &mut |k, _| {
            seen.push(k.to_vec());
            true
        })
        .unwrap();
        assert_eq!(seen.len(), 3);
        // stop signal honored
        let mut count = 0;
        for_each_from(&store, b"70/", false, &mut |_, _| {
            count += 1;
            count < 2
        })
        .unwrap();
        assert_eq!(count, 2);
        // exclusive resume skips the cursor key itself
        let mut resumed = Vec::new();
        for_each_from(&store, b"70/a", true, &mut |k, _| {
            resumed.push(k.to_vec());
            true
        })
        .unwrap();
        assert_eq!(resumed, vec![b"70/b".to_vec(), b"70/c".to_vec()]);
    }

    /// D9: reaching the range tail is a clean `Ok(())` -- the raw iterator
    /// is consumed as `Result` items now, so a mid-scan RocksDB error
    /// propagates as `Err` instead of silently truncating the scan (a
    /// store error cannot be injected cheaply here; the Err-shaped
    /// contract is exercised by the `Result<(), String>` signature and
    /// every caller already treats it as fallible).
    #[test]
    fn for_each_from_exhaustion_is_clean_ok() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"70/", b"a", b"1").unwrap();
        // full traversal ends Ok at the tail
        let mut seen = 0;
        assert_eq!(
            for_each_from(&store, b"70/", false, &mut |_, _| {
                seen += 1;
                true
            }),
            Ok(())
        );
        assert_eq!(seen, 1);
        // starting past every key also exhausts cleanly
        assert_eq!(for_each_from(&store, b"99/", false, &mut |_, _| true), Ok(()));
        // exclusive start on the last key visits nothing
        assert_eq!(for_each_from(&store, b"70/a", true, &mut |_, _| true), Ok(()));
    }

    #[test]
    fn first_key_from_bounds() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"70/", b"m", b"v").unwrap();
        assert_eq!(
            first_key_from(&store, b"70/").unwrap(),
            Some(b"70/m".to_vec())
        );
        assert_eq!(first_key_from(&store, b"70/n").unwrap(), None);
        assert_eq!(first_key_from(&store, b"").unwrap(), Some(b"70/m".to_vec()));
    }

    #[test]
    fn delete_range_paged_wipes_in_multiple_batches() {
        let (_dir, store) = open_tmp();
        for (k, v) in [
            ("a", b"1"),
            ("b", b"2"),
            ("c", b"3"),
            ("d", b"4"),
            ("e", b"5"),
        ] {
            rocksdb::set(&store, b"70/", k.as_bytes(), v).unwrap();
        }
        rocksdb::set(&store, b"90/", b"keep", b"x").unwrap(); // outside the window
        let upper = b"70/z".to_vec();
        // page=2: three deleting rounds (2 + 2 + 1), then an empty page.
        let (n1, c1) = delete_page_once(&store, b"70/a", &upper, 2).unwrap();
        assert_eq!((n1, c1.as_slice()), (2, b"70/c".as_slice()));
        let (n2, c2) = delete_page_once(&store, &c1, &upper, 2).unwrap();
        assert_eq!((n2, c2.as_slice()), (2, b"70/e".as_slice()));
        let (n3, c3) = delete_page_once(&store, &c2, &upper, 2).unwrap();
        assert_eq!((n3, c3.as_slice()), (1, b"70/e".as_slice()));
        let (n4, _) = delete_page_once(&store, &c3, &upper, 2).unwrap();
        assert_eq!(n4, 0);
        for k in ["a", "b", "c", "d", "e"] {
            assert_eq!(rocksdb::get(&store, b"70/", k.as_bytes()), Ok(None));
        }
        assert_eq!(
            rocksdb::get(&store, b"90/", b"keep"),
            Ok(Some(b"x".to_vec()))
        );
        // reseed: the packaged loop returns the same total
        for (k, v) in [
            ("a", b"1"),
            ("b", b"2"),
            ("c", b"3"),
            ("d", b"4"),
            ("e", b"5"),
        ] {
            rocksdb::set(&store, b"70/", k.as_bytes(), v).unwrap();
        }
        assert_eq!(
            delete_range_paged_inner(&store, b"70/a", &upper, 2).unwrap(),
            5
        );
    }

    #[test]
    fn delete_range_paged_empty_upper_wipes_to_end() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"10/", b"low", b"0").unwrap();
        rocksdb::set(&store, b"70/", b"a", b"1").unwrap();
        rocksdb::set(&store, b"99/", b"z", b"9").unwrap();
        assert_eq!(delete_range_paged_inner(&store, b"70/", b"", 2).unwrap(), 2);
        assert_eq!(rocksdb::get(&store, b"70/", b"a"), Ok(None));
        assert_eq!(rocksdb::get(&store, b"99/", b"z"), Ok(None)); // >= 70/
        assert_eq!(
            rocksdb::get(&store, b"10/", b"low"),
            Ok(Some(b"0".to_vec()))
        ); // < 70/
    }

    #[test]
    fn delete_range_paged_empty_range_returns_zero() {
        let (_dir, store) = open_tmp();
        rocksdb::set(&store, b"70/", b"a", b"1").unwrap();
        // window with nothing inside
        assert_eq!(
            delete_range_paged_inner(&store, b"80/", b"90/", 2).unwrap(),
            0
        );
        // open-ended range starting past the last key
        assert_eq!(
            delete_range_paged_inner(&store, b"70/b", b"", 2).unwrap(),
            0
        );
        assert_eq!(rocksdb::get(&store, b"70/", b"a"), Ok(Some(b"1".to_vec())));
    }

    #[tokio::test]
    async fn async_twins_match_sync_results() {
        let (_dir, store) = open_tmp();
        let shared = Arc::new(store);
        let mut batch = WriteBatch::default();
        batch.put(b"70/k", b"v");
        batch_write_async(Arc::clone(&shared), batch).await.unwrap();
        assert_eq!(get_physical(&shared, b"70/k").unwrap(), Some(b"v".to_vec()));
        delete_range_async(Arc::clone(&shared), b"70/k".to_vec(), b"70/l".to_vec())
            .await
            .unwrap();
        assert_eq!(get_physical(&shared, b"70/k").unwrap(), None);
        // paged async twin deletes to the same effect and reports the total
        let mut batch = WriteBatch::default();
        batch.put(b"70/p1", b"1");
        batch.put(b"70/p2", b"2");
        batch_write_async(Arc::clone(&shared), batch).await.unwrap();
        assert_eq!(
            delete_range_paged_async(Arc::clone(&shared), b"70/p".to_vec(), b"70/q".to_vec())
                .await
                .unwrap(),
            2
        );
        assert_eq!(get_physical(&shared, b"70/p2").unwrap(), None);
    }
}
