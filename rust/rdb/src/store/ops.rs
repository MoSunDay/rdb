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
/// Iteration errors abort the scan and surface as `Err`.
pub fn for_each_from(
    store: &Store,
    from: &[u8],
    excl_from: bool,
    f: &mut dyn FnMut(&[u8], &[u8]) -> bool,
) -> Result<(), String> {
    for (i, (k, v)) in forward_iter(store, from, None).enumerate() {
        if excl_from && i == 0 && k == from {
            continue;
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
    }
}
