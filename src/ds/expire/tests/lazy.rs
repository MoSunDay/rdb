//! Lazy-path tests: detached purge landing/cancellation and revalidation.
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::ds::codec::{KIND_STRING_TTL, STRING_FAMILY};
use crate::ds::expire::lazy::revalidate_expire;
use crate::store::ops;

#[test]
fn read_enveloped_missing_and_live() {
    let (_dir, store) = open_tmp("read");
    let store = Arc::new(store);
    assert_eq!(read_enveloped(&store, P, b"k").unwrap(), None);
    write_enveloped(&store, KIND_STRING_TTL, b"k", 9_000_000_000_000, b"val");
    let (expire, payload) = read_enveloped(&store, P, b"k").unwrap().unwrap();
    assert_eq!(expire, 9_000_000_000_000);
    assert_eq!(payload, b"val".to_vec());
}

#[test]
fn lazy_purge_deletes_family_and_index() {
    let (_dir, store) = open_tmp("lazy");
    write_enveloped(&store, KIND_STRING_TTL, b"k", 5, b"v");
    assert!(purge_if_expired(&store, P, STRING_FAMILY, b"k", 10));
    assert!(!purge_if_expired(&store, P, STRING_FAMILY, b"k", 10));
    assert_eq!(sample_once(&store, 10, 10, b"", None).0, 0);
    // an index entry whose record vanished is still swept
    let mut batch = WriteBatch::default();
    batch.put(
        codec::expire_index_key(P, 5, &codec::data_key(P, KIND_STRING_TTL, b"ghost")),
        b"",
    );
    ops::batch_write(&store, batch).unwrap();
    assert_eq!(sample_once(&store, 10, 10, b"", None).0, 1);
}

/// The Arc lazy-purge entry point commits via a detached off-worker
/// write: the DECISION returns immediately and the physical records
/// (family root + index entry) vanish shortly after.
#[tokio::test]
async fn purge_if_expired_arc_detached_write_lands() {
    let (_dir, store) = open_tmp("detach");
    let store = Arc::new(store);
    write_enveloped(&store, KIND_STRING_TTL, b"k", 5, b"v");
    let root = codec::data_key(P, KIND_STRING_TTL, b"k");
    let idx = codec::expire_index_key(P, 5, &root);
    // decision is reported at once; the physical commit is detached, so
    // an immediately repeated read may still see the due record and
    // re-decide (idempotent) -- only a landed write settles it
    assert!(purge_if_expired_arc(&store, P, STRING_FAMILY, b"k", 10));
    wait_until_gone(&store, &[root, idx]);
    assert!(!purge_if_expired_arc(&store, P, STRING_FAMILY, b"k", 10));
    // nothing left for the sampler to sweep
    assert_eq!(sample_once(&store, 10, 10, b"", None).0, 0);
}

/// read_enveloped's inline purge rides the same detached path.
#[tokio::test]
async fn read_enveloped_detached_purge_lands() {
    let (_dir, store) = open_tmp("readdet");
    let store = Arc::new(store);
    write_enveloped(&store, KIND_STRING_TTL, b"k", 5, b"v");
    assert_eq!(read_enveloped(&store, P, b"k").unwrap(), None);
    wait_until_gone(&store, &[codec::data_key(P, KIND_STRING_TTL, b"k")]);
    assert_eq!(read_enveloped(&store, P, b"k").unwrap(), None);
}

/// The detached-purge probe commits only on an UNCHANGED expire
/// deadline: a rewritten envelope (TTL extended or cleared) or a
/// vanished record cancels the commit.
#[test]
fn revalidation_probe_requires_unchanged_expire() {
    let probe = revalidate_expire(5);
    assert!(probe(Some(&codec::encode_envelope(5, b"v"))));
    assert!(!probe(Some(&codec::encode_envelope(
        9_000_000_000_000,
        b"v"
    ))));
    assert!(!probe(Some(&codec::encode_envelope(0, b"v"))));
    assert!(!probe(None));
}

/// TOCTOU regression: a racing writer that rewrites the key with a
/// live far-future envelope between the purge DECISION read and the
/// detached commit must not lose its record to the content-agnostic
/// family range deletes -- the revalidation read cancels the purge.
#[tokio::test]
async fn detached_purge_cancelled_when_value_replaced() {
    let (_dir, store) = open_tmp("race");
    let store = Arc::new(store);
    write_enveloped(&store, KIND_STRING_TTL, b"k", 5, b"old");
    let far = 9_000_000_000_000u64;
    // the decision is made on the expired envelope ...
    assert!(purge_if_expired_arc(&store, P, STRING_FAMILY, b"k", 10));
    // ... then the racing latched writer rewrites the key live before
    // the detached commit can land
    write_enveloped(&store, KIND_STRING_TTL, b"k", far, b"new");
    // whatever the interleaving, the far-future record must survive
    let root = codec::data_key(P, KIND_STRING_TTL, b"k");
    for _ in 0..200 {
        if let Some(val) = ops::get_physical(&store, &root).unwrap() {
            assert_eq!(codec::decode_envelope(&val), (far, b"new".as_slice()));
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("racing writer's far-future record was purged");
}
