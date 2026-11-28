//! Handler-level tests for the key-space commands over LITE STREAM NAMES
//! (batch-14 closure, form A): a valid `parent/child` name is stored by
//! the node-local X-* verbs under the PARENT slot, so EXISTS/TYPE/TTL/
//! DEL/RENAME must resolve it from the routed slot of the full name
//! (which differs from the parent's) -- and RENAME of two streams of one
//! parent moves the family under the parent slot, re-keying the cached
//! group states. Fixture rules as in `tests.rs`.

use super::*;
use crate::lite::model::{self, EntryId};
use crate::lite::offset::{self, GroupState};
use crate::state::Shared;

/// Routed slot prefix of the FULL name: what `dispatch` hands the handler.
fn routed(name: &[u8]) -> Vec<u8> {
    crate::hash::slot_with_prefix(crate::hash::hash_tag(name)).1
}

/// Parent slot prefix: where XADD stores the stream.
fn parent(name: &[u8]) -> Vec<u8> {
    crate::lite::model::stream_key_target(name).expect("valid stream name")
}

fn call(shared: &Shared, handler: crate::command::Handler, prefix: Vec<u8>, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let argv: Vec<Vec<u8>> = args.iter().map(|a| a.to_vec()).collect();
    let mut ctx = crate::command::test_ctx(shared, prefix, argv, &mut out);
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(handler(&mut ctx));
    out
}

/// Seed one live stream (meta + one entry) under its parent slot.
fn seed_stream(shared: &Shared, stream: &[u8]) {
    let prefix = parent(stream);
    let mut batch = rocksdb::WriteBatch::default();
    batch.put(
        model::meta_key(&prefix, stream),
        model::encode_meta_at(
            &model::MetaPayload {
                len: 1,
                ..Default::default()
            },
            0,
        ),
    );
    batch.put(
        model::entry_key(&prefix, stream, EntryId { ms: 1, seq: 0 }),
        model::encode_entry(&[(b"f", b"v")]),
    );
    crate::store::ops::batch_write(&shared.store, batch).expect("seed stream");
}

fn group_state(committed_ms: u64) -> GroupState {
    GroupState {
        created_ms: 1,
        delivered: EntryId { ms: 0, seq: 0 },
        committed: EntryId {
            ms: committed_ms,
            seq: 0,
        },
        pending: 0,
        ordered: false,
        inflight_max: 1,
        maxdelivery: 0,
        dlq: Vec::new(),
    }
}

fn gone(shared: &Shared, k: Vec<u8>) -> bool {
    crate::store::ops::get_physical(&shared.store, &k)
        .unwrap()
        .is_none()
}

#[test]
fn exists_type_del_see_stream() {
    let (_g, s) = super::tests::shared_for("127.0.0.1:40261");
    let name = b"orders/q0".as_slice();
    assert_ne!(routed(name), parent(name), "fixture: two distinct windows");
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), routed(name), &[name]),
        b":0\r\n".to_vec()
    );
    seed_stream(&s, name);
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), routed(name), &[name]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(type_(c)), routed(name), &[name]),
        b"+stream\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(ttl(c)), routed(name), &[name]),
        b":-1\r\n".to_vec()
    );
    // A plain key of the same name keeps winning in the routed window
    // (precedence: routed slot first, then the parent slot).
    crate::store::set(&s.store, &routed(name), name, b"str").expect("raw set");
    assert_eq!(
        call(&s, |c| Box::pin(type_(c)), routed(name), &[name]),
        b"+string\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(del(c)), routed(name), &[name]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(type_(c)), routed(name), &[name]),
        b"+stream\r\n".to_vec()
    );
    // DEL: the stream window goes, the cached group state is evicted
    // (no orphan group records on the next flush) and the deferred sweep
    // is queued under the PARENT prefix it needs.
    offset::insert_new(&s.lite.offsets, name, b"g", group_state(3));
    assert_eq!(
        call(&s, |c| Box::pin(del(c)), routed(name), &[name]),
        b":1\r\n".to_vec()
    );
    assert!(gone(&s, model::meta_key(&parent(name), name)));
    assert!(offset::peek_cached(&s.lite.offsets, name, b"g").is_none());
    assert_eq!(s.lite.pending_reaps(), 1);
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), routed(name), &[name]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(del(c)), routed(name), &[name]),
        b":0\r\n".to_vec()
    );
}

#[test]
fn rename_stream_ordinary_name_moves_entries_and_evicts_offsets() {
    let (_g, s) = super::tests::shared_for("127.0.0.1:40262");
    let (src, dst, group) = (
        b"orders/q0".as_slice(),
        b"orders/q9".as_slice(),
        b"g1".as_slice(),
    );
    seed_stream(&s, src);
    offset::insert_new(&s.lite.offsets, src, group, group_state(7));
    // The FULL names hash apart: the ordinary RENAME rule refuses them.
    assert!(!crate::ds::setops::same_slot(&[src.to_vec(), dst.to_vec()]));
    let ok = call(&s, |c| Box::pin(rename(c)), routed(src), &[src, dst]);
    assert_eq!(ok, b"+OK\r\n".to_vec());
    let p = parent(src);
    assert_eq!(p, parent(dst));
    assert!(gone(&s, model::meta_key(&p, src)));
    assert!(gone(
        &s,
        model::entry_key(&p, src, EntryId { ms: 1, seq: 0 })
    ));
    assert!(!gone(&s, model::meta_key(&p, dst)));
    assert!(!gone(
        &s,
        model::entry_key(&p, dst, EntryId { ms: 1, seq: 0 })
    ));
    // Cached group state RE-KEYED to the new name (unflushed deltas
    // survive) and marked dirty; nothing is left for the reaper.
    assert!(offset::peek_cached(&s.lite.offsets, src, group).is_none());
    let moved = offset::peek_cached(&s.lite.offsets, dst, group).expect("re-keyed group");
    assert_eq!(moved.committed, EntryId { ms: 7, seq: 0 });
    assert_eq!(
        offset::dirty_keys(&s.lite.offsets),
        vec![(dst.to_vec(), group.to_vec())]
    );
    assert_eq!(s.lite.pending_reaps(), 0);
    // Key-space view: old name gone, new name present from either slot.
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), routed(src), &[src]),
        b":0\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(exists(c)), routed(dst), &[dst]),
        b":1\r\n".to_vec()
    );
    assert_eq!(
        call(&s, |c| Box::pin(renamenx(c)), routed(dst), &[dst, dst]),
        b":1\r\n".to_vec(),
        "same name: an existence check"
    );
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), routed(src), &[src, dst]),
        b"-ERR no such key\r\n".to_vec()
    );
}

#[test]
fn rename_stream_window_rules() {
    let (_g, s) = super::tests::shared_for("127.0.0.1:40263");
    let (a, b) = (b"p1/q0".as_slice(), b"p2/q0".as_slice());
    seed_stream(&s, a);
    // Different parents -> CROSSSLOT, nothing moves.
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), routed(a), &[a, b]),
        b"-ERR CROSSSLOT Keys in request don't hash to the same slot\r\n".to_vec()
    );
    assert!(!gone(&s, model::meta_key(&parent(a), a)));
    // A stream name next to an ordinary key -> refused (two windows).
    let mixed = call(&s, |c| Box::pin(rename(c)), routed(a), &[a, b"plain"]);
    assert!(
        mixed.starts_with(b"-ERR RENAME source and destination must both be"),
        "{}",
        String::from_utf8_lossy(&mixed)
    );
    // Slash names that are NOT valid streams (hash tag) stay ordinary:
    // same hash-tag slot -> plain string move under the routed prefix.
    let (k1, k2) = (b"{o}/q0".as_slice(), b"{o}/q9".as_slice());
    crate::store::set(&s.store, &routed(k1), k1, b"v").expect("raw set");
    assert_eq!(
        call(&s, |c| Box::pin(rename(c)), routed(k1), &[k1, k2]),
        b"+OK\r\n".to_vec()
    );
    assert!(gone(&s, crate::ds::codec::string_key(&routed(k1), k1)));
    assert!(!gone(&s, crate::ds::codec::string_key(&routed(k2), k2)));
}
