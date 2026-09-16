//! Active-path tests: sampler purging, cursor resume/wrap semantics
//! and stream-reap offset-cache invalidation.
use std::sync::Arc;

use super::*;
use crate::ds::codec::{KIND_HASH_META, KIND_STRING_TTL};
use crate::ds::expire::active::SCAN_LIMIT;
use crate::store::ops;

#[test]
fn sampler_purges_due_entries_only() {
    let (_dir, store) = open_tmp("sample");
    let store = Arc::new(store);
    write_enveloped(&store, KIND_STRING_TTL, b"due", 100, b"v");
    write_enveloped(&store, KIND_STRING_TTL, b"later", 9_000_000_000_000, b"v");
    write_enveloped(&store, KIND_HASH_META, b"h", 50, b"meta");
    // element record under the hash family: must vanish with the meta
    rocksdb::set(&store, P, b"", b"").ok();
    let elem = codec::elem_key(P, crate::ds::codec::KIND_HASH_FLD, b"h", b"f");
    let mut batch = WriteBatch::default();
    batch.put(&elem, b"x");
    ops::batch_write(&store, batch).unwrap();

    assert_eq!(sample_once(&store, 200, 10, b"", None).0, 2);
    assert_eq!(read_enveloped(&store, P, b"due").unwrap(), None);
    // element went with the meta
    assert_eq!(ops::get_physical(&store, &elem).unwrap(), None);
    // not-yet-due untouched
    let (expire, payload) = read_enveloped(&store, P, b"later").unwrap().unwrap();
    assert_eq!((expire, payload), (9_000_000_000_000, b"v".to_vec()));
    // index entries for the purged keys are gone; a resample is idle
    assert_eq!(sample_once(&store, 200, 10, b"", None).0, 0);
}

#[test]
fn sampler_cursor_resumes_after_stop() {
    let (_dir, store) = open_tmp("resume");
    let store = Arc::new(store);
    write_enveloped_at(&store, b"10/", KIND_STRING_TTL, b"k", 100, b"v");
    write_enveloped_at(&store, b"99/", KIND_STRING_TTL, b"k", 100, b"v");
    // budget=1: the round purges the 10/ victim, then stops on the
    // next key it touches -- the 99/ data record, which stays
    // UNPROCESSED: the cursor must sit on the last key the round
    // actually processed (the 10/ index entry), never the stop key.
    let (purged, cursor) = sample_once(&store, 200, 1, b"", None);
    assert_eq!(purged, 1);
    assert_eq!(
        cursor,
        codec::expire_index_key(b"10/", 100, &codec::data_key(b"10/", KIND_STRING_TTL, b"k")),
        "cursor sits on the last key the round PROCESSED"
    );
    // resuming after that cursor clears the 99/ victim in slot order
    let (purged2, cursor2) = sample_once(&store, 200, 1, &cursor, None);
    assert_eq!(purged2, 1);
    assert!(cursor2.is_empty(), "scan reached the tail and wrapped");
    assert_eq!(read_enveloped(&store, b"10/", b"k").unwrap(), None);
    assert_eq!(read_enveloped(&store, b"99/", b"k").unwrap(), None);
}

/// The stop key of a budget-cut round is re-examined -- not skipped --
/// by the next round: two due keys in one slot, budget=1, so round 1
/// halts exactly ON k2's index entry.
#[test]
fn sampler_stop_key_is_not_skipped_next_round() {
    let (_dir, store) = open_tmp("stopkey");
    let store = Arc::new(store);
    write_enveloped(&store, KIND_STRING_TTL, b"k1", 100, b"v");
    write_enveloped(&store, KIND_STRING_TTL, b"k2", 100, b"v");
    let (purged1, cursor1) = sample_once(&store, 200, 1, b"", None);
    assert_eq!(purged1, 1);
    assert!(!cursor1.is_empty(), "budget cut the round: cursor returned");
    // round 2 must land on k2's index entry -- the key round 1
    // stopped on -- instead of resuming strictly after it
    let (purged2, cursor2) = sample_once(&store, 200, 1, &cursor1, None);
    assert_eq!(purged2, 1, "the stop key was re-examined, not skipped");
    assert!(cursor2.is_empty(), "both victims gone: scan hit the tail");
    let (purged3, _) = sample_once(&store, 200, 1, &cursor2, None);
    assert_eq!(purged3, 0);
    for key in [b"k1".as_slice(), b"k2".as_slice()] {
        assert_eq!(
            ops::get_physical(&store, &codec::data_key(P, KIND_STRING_TTL, key)).unwrap(),
            None
        );
    }
}

#[test]
fn sampler_cursor_wraps_after_tail() {
    let (_dir, store) = open_tmp("wrap");
    let store = Arc::new(store);
    write_enveloped_at(&store, b"99/", KIND_STRING_TTL, b"tail", 100, b"v");
    let (purged, cursor) = sample_once(&store, 200, 10, b"", None);
    assert_eq!(purged, 1);
    assert!(
        cursor.is_empty(),
        "natural exhaustion returns an empty cursor"
    );
    // a NEW victim sorting before everything the sweep just saw: only
    // a wrapped (head restart) round can reach it
    write_enveloped_at(&store, b"10/", KIND_STRING_TTL, b"head", 100, b"v");
    let (purged2, cursor2) = sample_once(&store, 200, 10, &cursor, None);
    assert_eq!(purged2, 1, "wrapped round restarts from the head");
    assert!(cursor2.is_empty());
    assert_eq!(read_enveloped(&store, b"10/", b"head").unwrap(), None);
    assert_eq!(read_enveloped(&store, b"99/", b"tail").unwrap(), None);
}

#[test]
fn sampler_reaches_keys_past_scan_limit() {
    let (_dir, store) = open_tmp("limit");
    let store = Arc::new(store);
    // >SCAN_LIMIT live records at slot 10/ push the cursor forward one
    // SCAN_LIMIT-window per round (written in one batch to keep the
    // test off the fsync path).
    let far = 9_000_000_000_000u64;
    let mut batch = WriteBatch::default();
    for i in 0..=SCAN_LIMIT {
        let key = format!("bulk{i:04}").into_bytes();
        let root = codec::data_key(b"10/", KIND_STRING_TTL, &key);
        batch.put(&root, codec::encode_envelope(far, b"v"));
        batch.put(codec::expire_index_key(b"10/", far, &root), b"");
    }
    ops::batch_write(&store, batch).unwrap();
    // the due key sits in a slot BEYOND the bulk window
    write_enveloped_at(&store, b"99/", KIND_STRING_TTL, b"due", 100, b"v");
    let due = codec::data_key(b"99/", KIND_STRING_TTL, b"due");

    let mut cursor = Vec::new();
    let mut rounds = 0;
    while ops::get_physical(&store, &due).unwrap().is_some() {
        rounds += 1;
        assert!(rounds < 16, "cursor rotation never reached the high slot");
        let (_, next) = sample_once(&store, 200, 20, &cursor, None);
        cursor = next;
    }
    assert!(rounds > 1, "the scan limit forced multiple rounds");
    // the live bulk records were only ever passed over, never purged
    assert_eq!(
        read_enveloped(&store, b"10/", b"bulk0000")
            .unwrap()
            .unwrap()
            .0,
        far
    );
}

#[test]
fn sampler_reap_of_stream_invalidates_offset_cache() {
    let (_dir, store) = open_tmp("streamreap");
    let store = Arc::new(store);
    let rt = crate::lite::new_runtime();

    // One lite stream `p/c` stored under the PARENT-derived slot
    // prefix: a meta record whose idle deadline is long past (+ its
    // expire-index entry), one group record, and a DIRTY cached
    // group offset for it (one XACK's worth -- the resurrection
    // trigger the fix must defuse).
    let prefix = crate::hash::slot_with_prefix(b"p").1;
    let stream = b"p/c".to_vec();
    let mkey = crate::lite::model::meta_key(&prefix, &stream);
    let gkey = crate::lite::model::group_key(&prefix, &stream, b"g");
    let mut batch = WriteBatch::default();
    batch.put(
        &mkey,
        crate::lite::model::encode_meta_at(
            &crate::lite::model::MetaPayload {
                created_ms: 1,
                last_ms: 1,
                last_seq: 0,
                len: 1,
                idle_ms: 60_000,
            },
            1, // deadline at epoch+1ms: due since forever
        ),
    );
    batch.put(codec::expire_index_key(&prefix, 1, &mkey), b"");
    batch.put(
        &gkey,
        crate::lite::model::encode_group(&crate::lite::model::GroupPayload {
            created_ms: 1,
            delivered_ms: 1,
            delivered_seq: 0,
            committed_ms: 1,
            committed_seq: 0,
            ordered: false,
            inflight_max: 0,
        }),
    );
    ops::batch_write(&store, batch).unwrap();

    crate::lite::offset::insert_new(
        &rt.offsets,
        &stream,
        b"g",
        crate::lite::offset::GroupState {
            created_ms: 1,
            delivered: crate::lite::model::EntryId { ms: 1, seq: 0 },
            committed: crate::lite::model::EntryId { ms: 1, seq: 0 },
            pending: 0,
            ordered: false,
            inflight_max: 0,
        },
    );
    crate::lite::offset::ack(
        &rt.offsets,
        &stream,
        b"g",
        &[crate::lite::model::EntryId { ms: 2, seq: 0 }],
        None,
    )
    .unwrap();
    assert_eq!(crate::lite::offset::dirty_len(&rt.offsets), 1);

    // The sampler purges the due family AND reports the stream for
    // offset-cache invalidation + the deferred orphan sweep.
    let (purged, _) = sample_once(&store, now_ms(), 10, b"", Some(&rt));
    assert_eq!(purged, 1, "the due stream family was reaped");
    assert!(
        ops::get_physical(&store, &gkey).unwrap().is_none(),
        "family delete removed the group record"
    );
    assert_eq!(rt.pending_reaps(), 1, "reap queued the latched sweep");
    assert_eq!(
        crate::lite::offset::dirty_len(&rt.offsets),
        0,
        "cached dirty group state dropped with the family"
    );

    // A not-yet-due stream family is passed over and never queued.
    let live = crate::lite::model::meta_key(&prefix, b"p/live");
    let mut batch = WriteBatch::default();
    batch.put(
        &live,
        crate::lite::model::encode_meta_at(
            &crate::lite::model::MetaPayload {
                created_ms: 1,
                last_ms: 1,
                last_seq: 0,
                len: 0,
                idle_ms: 60_000,
            },
            now_ms() + 60_000,
        ),
    );
    batch.put(
        codec::expire_index_key(&prefix, now_ms() + 60_000, &live),
        b"",
    );
    ops::batch_write(&store, batch).unwrap();
    let (purged, _) = sample_once(&store, now_ms(), 10, b"", Some(&rt));
    assert_eq!(purged, 0);
    assert_eq!(rt.pending_reaps(), 1, "live families never queue sweeps");
}
