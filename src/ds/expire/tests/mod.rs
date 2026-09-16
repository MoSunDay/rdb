//! Shared test fixtures plus core-helper tests; the lazy and active
//! path tests live in sibling modules.
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::ds::codec::KIND_STRING_TTL;
use crate::store::ops;
use crate::store::rocksdb;
use crate::store::Store;

mod active;
mod lazy;

fn open_tmp(_tag: &str) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = rocksdb::open(dir.path().to_str().unwrap()).expect("open");
    (dir, store)
}

const P: &[u8] = b"70/";

fn write_enveloped(store: &Store, kind: u8, key: &[u8], expire: u64, payload: &[u8]) {
    write_enveloped_at(store, P, kind, key, expire, payload);
}

fn write_enveloped_at(
    store: &Store,
    prefix: &[u8],
    kind: u8,
    key: &[u8],
    expire: u64,
    payload: &[u8],
) {
    let root = codec::data_key(prefix, kind, key);
    let mut batch = WriteBatch::default();
    batch.put(&root, codec::encode_envelope(expire, payload));
    set_ttl_entries(&mut batch, prefix, root, 0, expire);
    ops::batch_write(store, batch).unwrap();
}

/// Bounded sleep-poll helper: returns once `gone(store, keys)` holds
/// for every key, panicking after ~2s if a detached write never lands.
fn wait_until_gone(store: &Arc<Store>, keys: &[Vec<u8>]) {
    for _ in 0..200 {
        if keys
            .iter()
            .all(|k| ops::get_physical(store, k).unwrap().is_none())
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("detached purge write never landed");
}

#[test]
fn is_expired_zero_never() {
    assert!(!is_expired(0, u64::MAX));
    assert!(!is_expired(10, 9));
    assert!(is_expired(10, 10));
    assert!(is_expired(10, 11));
}

#[test]
fn ttl_removed_cleans_index() {
    let (_dir, store) = open_tmp("ttlrm");
    let store = Arc::new(store);
    write_enveloped(&store, KIND_STRING_TTL, b"k", 111, b"v");
    let root = codec::data_key(P, KIND_STRING_TTL, b"k");
    let mut batch = WriteBatch::default();
    batch.put(&root, codec::encode_envelope(0, b"v"));
    set_ttl_entries(&mut batch, P, root, 111, 0);
    ops::batch_write(&store, batch).unwrap();
    assert_eq!(sample_once(&store, 500, 10, b"", None).0, 0);
    let (expire, payload) = read_enveloped(&store, P, b"k").unwrap().unwrap();
    assert_eq!((expire, payload), (0, b"v".to_vec()));
}

#[test]
fn slot_prefix_len_parses() {
    assert_eq!(slot_prefix_len(b"70/\x02"), Some(3));
    assert_eq!(slot_prefix_len(b"0/x"), Some(2));
    assert_eq!(slot_prefix_len(b"16383/y"), Some(6));
    assert_eq!(slot_prefix_len(b"noblash"), None);
    assert_eq!(slot_prefix_len(b"123456/"), None); // 6 digits: not a slot
    assert_eq!(slot_prefix_len(b""), None);
    assert_eq!(slot_prefix_len(b"70"), None);
}
