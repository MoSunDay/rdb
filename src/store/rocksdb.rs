//! RocksDB-backed store (Rust port of Go `internal/store/pebble.go`).
//!
//! Engine differs from the Go original (RocksDB instead of pebble), but the
//! physical key format is identical: `prefix + key`, where the caller supplies
//! the prefix (normally `"<decimal-slot>/"`, see [`slot_prefix`]). All writes
//! are synchronous, mirroring Go's `pebble.Sync` on every operation.
//!
//! Bug fixes vs the Go implementation (agreed deviations):
//! - [`del`] returns whether the key existed; Go ignored the delete error and
//!   always replied success, so DEL always answered `1`.
//! - [`size`] returns `rocksdb.estimate-num-keys`; Go's `Size()` created an
//!   empty batch and returned its length, i.e. always `0`.

use std::path::PathBuf;
use std::sync::Arc;

use rocksdb::{
    BlockBasedOptions, Direction, IteratorMode, Options, ReadOptions, WriteBatch, WriteOptions, DB,
};

/// Thin wrapper around a RocksDB handle. Plain data carrier only; all logic
/// lives in the free functions of this module.
///
/// Closing: `rocksdb::DB` releases its resources via `Drop`, so dropping a
/// `Store` closes the database (see [`close`]).
pub struct Store {
    /// `pub(crate)` so the sibling `store::ops` module can drive iterators,
    /// range deletes and batched writes directly.
    pub(crate) db: DB,
}

/// Compile-time proof that `Store` can be shared across threads/tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Store>();
};

/// Open (or create) a RocksDB database at `path`.
///
/// Options mirror Go's `OpenPebble`: `create_if_missing` plus a block-based
/// table factory with a bloom filter of 10 bits per key (`block_based = false`
/// selects the full bloom filter, equivalent to pebble's
/// `bloom.FilterPolicy(10)` which Go installed on every level).
pub fn open(path: &str) -> Result<Store, String> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    // Write-path tuning (A/B-verified on the ZFS fsync floor: mainly buys
    // tail-latency headroom by keeping flush/compaction off the write path).
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    // increase_parallelism also raises max_background_jobs internally.
    opts.increase_parallelism(cpus as i32);
    opts.set_write_buffer_size(64 << 20);
    opts.set_enable_pipelined_write(true);
    opts.set_max_write_buffer_number(4);
    let mut table_opts = BlockBasedOptions::default();
    table_opts.set_bloom_filter(10.0, false);
    opts.set_block_based_table_factory(&table_opts);
    DB::open(&opts, path)
        .map(|db| Store { db })
        .map_err(|e| e.to_string())
}

/// Explicitly close a store. `rocksdb::DB` already closes via `Drop`, so this
/// only exists to mirror Go's `Close()`; a plain drop has the same effect.
pub fn close(store: Store) {
    drop(store);
}

/// Mirror Go's `filepath.Join(store_path, bind)`: join the two components with
/// `/` and clean the result (collapse duplicate slashes, resolve `.`/`..`).
pub fn data_path(store_path: &str, bind: &str) -> PathBuf {
    let joined = if store_path.is_empty() {
        bind.to_string()
    } else if bind.is_empty() {
        store_path.to_string()
    } else {
        format!("{store_path}/{bind}")
    };
    let absolute = joined.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if !parts.is_empty() && parts[parts.len() - 1] != ".." {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }
    let mut cleaned = if absolute {
        String::from("/")
    } else {
        String::new()
    };
    cleaned.push_str(&parts.join("/"));
    if cleaned.is_empty() {
        cleaned.push('.');
    }
    PathBuf::from(cleaned)
}

/// Physical key layout shared with the Go implementation: `prefix ++ key`.
fn physical_key(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    let mut item = Vec::with_capacity(prefix.len() + key.len());
    item.extend_from_slice(prefix);
    item.extend_from_slice(key);
    item
}

/// WriteOptions with `sync = true` -- Go uses `pebble.Sync` everywhere.
pub(crate) fn sync_write_opts() -> WriteOptions {
    let mut wopts = WriteOptions::default();
    wopts.set_sync(true);
    wopts
}

/// Get a value. Missing keys map to `Ok(None)`; real errors to `Err` (Go
/// returns the error to the caller, which replies with a null bulk string).
pub fn get(store: &Store, prefix: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, String> {
    store
        .db
        .get(physical_key(prefix, key))
        .map_err(|e| e.to_string())
}

/// Synchronously set a value.
pub fn set(store: &Store, prefix: &[u8], key: &[u8], val: &[u8]) -> Result<(), String> {
    store
        .db
        .put_opt(physical_key(prefix, key), val, &sync_write_opts())
        .map_err(|e| e.to_string())
}

/// Delete a key, returning whether it existed.
///
/// FIX vs Go: Go's `Del` ignored the delete error and always returned nil, so
/// DEL always replied `1`. Here the caller gets the real `0/1` answer. The
/// existence check and the delete are two operations (check-then-delete), so a
/// concurrent writer racing in between could skew the answer; this is
/// acceptable because Redis DEL semantics are best-effort for single keys.
pub fn del(store: &Store, prefix: &[u8], key: &[u8]) -> Result<bool, String> {
    let item = physical_key(prefix, key);
    let existed = store.db.get(&item).map_err(|e| e.to_string())?.is_some();
    if existed {
        store
            .db
            .delete_opt(&item, &sync_write_opts())
            .map_err(|e| e.to_string())?;
    }
    Ok(existed)
}

/// Multi-get. On any error or miss the entry is an empty `Vec` -- Go appends
/// `[]byte{}` for every failed lookup, which is indistinguishable from a
/// stored empty value; that behaviour is preserved on purpose.
pub fn mget(store: &Store, prefix: &[u8], keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
    keys.iter()
        .map(|key| match get(store, prefix, key) {
            Ok(Some(val)) => val,
            _ => Vec::new(),
        })
        .collect()
}

/// Atomically write key/value pairs in a single synchronous batch.
///
/// FIX vs Go: Go's `MSet` indexed `data[i+1]` and panicked on odd-length
/// input. Here odd length is rejected explicitly with `Err` in both debug and
/// release builds (a `debug_assert!` would panic in the debug test profile and
/// mask the `Err` contract, so the runtime check is the single enforcement).
pub fn mset(store: &Store, prefix: &[u8], pairs: &[Vec<u8>]) -> Result<(), String> {
    if !pairs.len().is_multiple_of(2) {
        return Err("mset: odd number of elements, expected key/value pairs".to_string());
    }
    let mut batch = WriteBatch::default();
    for pair in pairs.chunks_exact(2) {
        batch.put(physical_key(prefix, &pair[0]), &pair[1]);
    }
    store
        .db
        .write_opt(batch, &sync_write_opts())
        .map_err(|e| e.to_string())
}

/// Async twin of [`set`]: the blocking fsync runs on tokio's blocking pool
/// so it never stalls a worker (Go parity: a goroutine blocked in
/// `pebble.Sync` simply grows the Go runtime). Durability is unchanged --
/// the same `sync = true` WriteOptions is used, just on another thread.
pub async fn set_async(
    store: Arc<Store>,
    prefix: Vec<u8>,
    key: Vec<u8>,
    val: Vec<u8>,
) -> Result<(), String> {
    let item = physical_key(&prefix, &key);
    tokio::task::spawn_blocking(move || store.db.put_opt(item, val, &sync_write_opts()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// Async twin of [`del`] (same check-then-delete shape, fsync off-worker).
pub async fn del_async(store: Arc<Store>, prefix: Vec<u8>, key: Vec<u8>) -> Result<bool, String> {
    let item = physical_key(&prefix, &key);
    tokio::task::spawn_blocking(move || {
        let existed = store.db.get(&item).map_err(|e| e.to_string())?.is_some();
        if existed {
            store
                .db
                .delete_opt(&item, &sync_write_opts())
                .map_err(|e| e.to_string())?;
        }
        Ok(existed)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Async twin of [`mset`] (one batched sync write, fsync off-worker).
pub async fn mset_async(
    store: Arc<Store>,
    prefix: Vec<u8>,
    pairs: Vec<Vec<u8>>,
) -> Result<(), String> {
    if !pairs.len().is_multiple_of(2) {
        return Err("mset: odd number of elements, expected key/value pairs".to_string());
    }
    tokio::task::spawn_blocking(move || {
        let mut batch = WriteBatch::default();
        for pair in pairs.chunks_exact(2) {
            batch.put(physical_key(&prefix, &pair[0]), &pair[1]);
        }
        store
            .db
            .write_opt(batch, &sync_write_opts())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Estimated number of keys.
///
/// FIX vs Go: Go's `Size()` created an empty batch and returned its length,
/// so it always returned `0`. Here the RocksDB property
/// `rocksdb.estimate-num-keys` is used; failures default to `0`.
pub fn size(store: &Store) -> u64 {
    store
        .db
        .property_int_value("rocksdb.estimate-num-keys")
        .ok()
        .flatten()
        .unwrap_or(0)
}

/// Upper bound for a prefix range scan; `None` means no upper bound.
///
/// Exact port of the Go closure in `Pebble.Iter`: increment the last byte; on
/// wrap-around keep incrementing the previous byte and truncate after the
/// first non-wrapped byte. All-`0xff` input wraps completely -> `None`.
pub fn key_upper_bound(b: &[u8]) -> Option<Vec<u8>> {
    let mut end = b.to_vec();
    for i in (0..end.len()).rev() {
        end[i] = end[i].wrapping_add(1);
        if end[i] != 0 {
            end.truncate(i + 1);
            return Some(end);
        }
    }
    None // no upper bound
}

/// Materialize every key in `[prefix, key_upper_bound(prefix))` -- Rust port of
/// Go's `Pebble.Iter` range (`LowerBound: prefix`, `UpperBound:
/// keyUpperBound(prefix)`). Iteration errors stop collection, matching Go's
/// iterator which also stops on error.
pub fn iter_prefix(store: &Store, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut ropts = ReadOptions::default();
    ropts.set_iterate_lower_bound(prefix.to_vec());
    if let Some(upper) = key_upper_bound(prefix) {
        ropts.set_iterate_upper_bound(upper);
    }
    let mode = IteratorMode::From(prefix, Direction::Forward);
    store
        .db
        .iterator_opt(mode, ropts)
        .map_while(Result::ok)
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect()
}

/// Prefix for a cluster slot: decimal ASCII + `/` (e.g. slot 5465 -> "5465/").
/// Helper for callers so they never format the prefix by hand.
pub fn slot_prefix(slot: u16) -> Vec<u8> {
    let mut p = slot.to_string().into_bytes();
    p.push(b'/');
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path().to_str().unwrap()).expect("open rocksdb");
        (dir, store)
    }

    #[test]
    fn open_set_get_roundtrip_incl_binary() {
        let (_dir, store) = open_tmp();
        let prefix = slot_prefix(5465);
        assert_eq!(prefix, b"5465/".to_vec());

        // plain key/value
        set(&store, &prefix, b"foo", b"bar").unwrap();
        assert_eq!(get(&store, &prefix, b"foo"), Ok(Some(b"bar".to_vec())));

        // binary key/value containing 0x00 bytes
        let bkey: &[u8] = b"a\x00b\x00";
        let bval: &[u8] = b"\x00\x01\xff\x00";
        set(&store, &prefix, bkey, bval).unwrap();
        assert_eq!(get(&store, &prefix, bkey), Ok(Some(bval.to_vec())));

        // overwrite
        set(&store, &prefix, b"foo", b"baz").unwrap();
        assert_eq!(get(&store, &prefix, b"foo"), Ok(Some(b"baz".to_vec())));
    }

    #[test]
    fn get_missing_is_none() {
        let (_dir, store) = open_tmp();
        assert_eq!(get(&store, b"5465/", b"nope"), Ok(None));
    }

    #[test]
    fn physical_key_format_and_prefix_scan() {
        let (_dir, store) = open_tmp();
        set(&store, b"5465/", b"foo", b"x").unwrap();
        set(&store, b"5466/", b"bar", b"y").unwrap();

        // raw read through the same prefix/key split
        assert_eq!(get(&store, b"5465/", b"foo"), Ok(Some(b"x".to_vec())));

        // prefix scan sees exactly the 5465 pair, not the 5466 one
        let items = iter_prefix(&store, b"5465/");
        assert_eq!(items, vec![(b"5465/foo".to_vec(), b"x".to_vec())]);

        let items = iter_prefix(&store, b"5466/");
        assert_eq!(items, vec![(b"5466/bar".to_vec(), b"y".to_vec())]);
    }

    #[test]
    fn mset_atomic_visible_and_odd_length_rejected() {
        let (_dir, store) = open_tmp();
        let pairs: Vec<Vec<u8>> = vec![
            b"k1".to_vec(),
            b"v1".to_vec(),
            b"k2".to_vec(),
            b"v2".to_vec(),
            b"k3".to_vec(),
            b"v3".to_vec(),
        ];
        mset(&store, b"70/", &pairs).unwrap();
        assert_eq!(get(&store, b"70/", b"k1"), Ok(Some(b"v1".to_vec())));
        assert_eq!(get(&store, b"70/", b"k2"), Ok(Some(b"v2".to_vec())));
        assert_eq!(get(&store, b"70/", b"k3"), Ok(Some(b"v3".to_vec())));

        let odd: Vec<Vec<u8>> = vec![b"k".to_vec(), b"v".to_vec(), b"dangling".to_vec()];
        assert!(mset(&store, b"70/", &odd).is_err());
    }

    #[test]
    fn mget_missing_becomes_empty_entry() {
        let (_dir, store) = open_tmp();
        set(&store, b"70/", b"present", b"value").unwrap();
        set(&store, b"70/", b"empty", b"").unwrap();

        let keys = vec![b"present".to_vec(), b"absent".to_vec(), b"empty".to_vec()];
        let got = mget(&store, b"70/", &keys);
        assert_eq!(
            got,
            vec![b"value".to_vec(), Vec::<u8>::new(), Vec::<u8>::new()]
        );
    }

    #[test]
    fn del_reports_existence() {
        let (_dir, store) = open_tmp();
        set(&store, b"70/", b"k", b"v").unwrap();
        assert_eq!(del(&store, b"70/", b"k"), Ok(true));
        assert_eq!(get(&store, b"70/", b"k"), Ok(None));
        assert_eq!(del(&store, b"70/", b"k"), Ok(false));
        assert_eq!(del(&store, b"70/", b"never-existed"), Ok(false));
    }

    #[test]
    fn size_counts_keys() {
        let (_dir, store) = open_tmp();
        assert_eq!(size(&store), 0);
        set(&store, b"70/", b"a", b"1").unwrap();
        set(&store, b"70/", b"b", b"2").unwrap();
        assert!(size(&store) >= 1, "estimate-num-keys should see the writes");
    }

    #[test]
    fn key_upper_bound_matches_go_loop() {
        assert_eq!(key_upper_bound(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(key_upper_bound(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(key_upper_bound(b"\xff\xff"), None);
        assert_eq!(key_upper_bound(b""), None);
        // multi-byte carry then truncate: "a\xff\xff" -> "b"
        assert_eq!(key_upper_bound(b"a\xff\xff"), Some(b"b".to_vec()));
    }

    #[test]
    fn data_path_mirrors_go_filepath_join() {
        assert_eq!(
            data_path("/tmp/", "127.0.0.1:32681"),
            PathBuf::from("/tmp/127.0.0.1:32681")
        );
        assert_eq!(data_path("/tmp", "x"), PathBuf::from("/tmp/x"));
        assert_eq!(data_path("/tmp//", "x/"), PathBuf::from("/tmp/x"));
        assert_eq!(data_path("/tmp/a", ".."), PathBuf::from("/tmp"));
    }

    /// Engine-evaluation record (documentation-by-test; see COMPAT.md
    /// "Transactions"): rust-rocksdb 0.24's OptimisticTransactionDB was
    /// evaluated for EXEC and rejected on three measured grounds:
    ///
    /// 1. A base (non-tx) write racing an open OCC transaction makes the
    ///    transaction's commit fail with "Resource busy" -- regardless of
    ///    `enable_pipelined_write`. Since non-transactional writes are the
    ///    hot path here (transactions are opt-in per request), every
    ///    concurrent plain write would abort in-flight transactions.
    /// 2. The transactional WriteBatch variant (`<true>`) lacks
    ///    `delete_range`, which the ds-layer family deletes depend on.
    /// 3. Staging writes inside a transaction hides them from the command
    ///    read path (no read-your-writes), breaking RMW chains like
    ///    MULTI; INCR; INCR; EXEC.
    ///
    /// EXEC isolation is therefore enforced at the application level with
    /// byte-sorted key latches (`tx::exec`). If this test ever fails,
    /// upstream changed premise (1) -- re-evaluate the engine then.
    #[test]
    fn occ_engine_evaluation_record() {
        use rocksdb::OptimisticTransactionDB;

        let race = |pipelined: bool| -> String {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut opts = Options::default();
            opts.create_if_missing(true);
            opts.set_enable_pipelined_write(pipelined);
            let db: OptimisticTransactionDB =
                OptimisticTransactionDB::open(&opts, dir.path().to_str().unwrap()).unwrap();
            // an isolated transaction opens and commits fine
            let solo = db.transaction();
            solo.put(b"solo", b"1").unwrap();
            solo.commit().expect("isolated commit works");
            // ...but a racing base write breaks the commit path
            let txn = db.transaction();
            txn.put(b"k", b"txn").unwrap();
            db.put_opt(b"k", b"raced", &sync_write_opts()).unwrap();
            match txn.commit() {
                Ok(()) => "committed".to_string(),
                Err(e) => e.to_string(),
            }
        };
        assert_eq!(race(true), "Resource busy: ");
        assert_eq!(race(false), "Resource busy: ");
    }

    #[test]
    fn close_drops_store() {
        let (dir, store) = open_tmp();
        set(&store, b"70/", b"k", b"v").unwrap();
        close(store);
        // reopening the same dir must succeed (handle really released)
        let store2 = open(dir.path().to_str().unwrap()).expect("reopen");
        assert_eq!(get(&store2, b"70/", b"k"), Ok(Some(b"v".to_vec())));
    }
}
