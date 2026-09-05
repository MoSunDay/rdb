//! Stream-reap x offset-cache e2e: every family-deletion path that
//! removes lite stream records OUTSIDE the normal command flow (XIDLE
//! active-expire reap, lazy idle purge on read, DEL/EXPIRE) must
//! invalidate the cached group offsets. Pins the contract pair:
//! (1) lazy idle purge on read drops the stream's cached state NOW and
//!     queues the latched orphan sweep, so the 200ms offset flusher --
//!     whose `drop_superseded` only re-checks the cache's own map --
//!     cannot resurrect group records onto the reaped family;
//! (2) the sweep deletes even a group record the flusher already wrote
//!     (planted here directly), so a same-name XGROUP CREATE succeeds
//!     instead of hitting BUSYGROUP forever.

mod common;

use std::sync::Arc;

use common::lite::{call, shared_at, text};
use rdb::state::Shared;

/// One offset flush round, exactly like the 200ms background loop.
fn flush_offsets(shared: &Arc<Shared>) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(rdb::lite::flush_offsets_once(shared))
        .expect("offset flush round");
}

/// Run one deferred-reap drain, exactly like the background loops do.
fn drain_reaps(shared: &Arc<Shared>) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("test runtime")
        .block_on(rdb::lite::drain_reaps(shared));
}

#[test]
fn lazy_idle_purge_invalidates_offset_cache_and_sweep_kills_orphans() {
    let shared = Arc::new(shared_at("43871").0);

    // ---- seed: one entry, group g1 delivers and acks it (state cached
    // and marked dirty exactly like a live consumer workload) ----
    assert!(text(&call(&shared, "xadd", &[b"s/qr", b"1-1", b"f", b"v"])).contains("1-1"));
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qr", b"g1", b"0-0"]),
        b"+OK\r\n".to_vec()
    );
    let delivered = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g1", b"c1", b"streams", b"s/qr", b">"],
    ));
    assert!(delivered.contains("1-1"), "{delivered}");
    assert_eq!(
        call(&shared, "xack", &[b"s/qr", b"g1", b"1-1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(rdb::lite::offset::dirty_len(&shared.lite.offsets), 1);

    // ---- arm a 1s idle TTL, let it lapse, then read: the lazy purge
    // inside read_meta must run the NOW-invalidation + reap queueing
    // even though the group state sits dirty in the cache ----
    assert_eq!(
        call(&shared, "xidle", &[b"s/qr", b"1"]),
        b"+OK\r\n".to_vec()
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let info = text(&call(&shared, "xinfo", &[b"stream", b"s/qr"]));
    assert!(info.contains("no such key"), "{info}");
    assert_eq!(
        rdb::lite::offset::dirty_len(&shared.lite.offsets),
        0,
        "lazy purge must drop the cached group state NOW"
    );
    assert_eq!(
        shared.lite.pending_reaps(),
        1,
        "lazy purge must queue the orphan sweep"
    );

    // ---- worst case the sweep exists for: a flush round had already
    // passed validation when the reap happened and wrote the group
    // record onto the reaped family. Plant that orphan directly ----
    let prefix = rdb::hash::slot_with_prefix(b"s").1;
    let mut batch = rocksdb::WriteBatch::default();
    batch.put(
        rdb::lite::model::group_key(&prefix, b"s/qr", b"g1"),
        rdb::lite::model::encode_group(&rdb::lite::model::GroupPayload {
            created_ms: 1,
            delivered_ms: 1,
            delivered_seq: 1,
            committed_ms: 1,
            committed_seq: 1,
        }),
    );
    rdb::store::ops::batch_write(&shared.store, batch).expect("plant orphan");
    drain_reaps(&shared);
    assert_eq!(shared.lite.pending_reaps(), 0, "sweep ran");

    // ---- the reaped family is CLEAN: same-name group recreates (no
    // BUSYGROUP), and a flush round resurrects nothing ----
    // A fresh XADD rebuilds the stream meta; the group record must be
    // gone, so re-creating the SAME group name succeeds instead of the
    // pre-fix permanent BUSYGROUP.
    assert!(text(&call(&shared, "xadd", &[b"s/qr", b"2-1", b"f", b"v"])).contains("2-1"));
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qr", b"g1", b"0-0"]),
        b"+OK\r\n".to_vec(),
        "recreated family must not report BUSYGROUP"
    );
    flush_offsets(&shared);
    // The fresh family is a NEW incarnation: delivering through the
    // re-created group works end-to-end, and one more flush round
    // writes its state without resurrecting the planted orphan (the
    // sweep already deleted it; the cache holds exactly this one
    // group's fresh state, which drops clean once written).
    let redelivered = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g1", b"c1", b"streams", b"s/qr", b">"],
    ));
    assert!(redelivered.contains("2-1"), "{redelivered}");
    assert_eq!(
        call(&shared, "xack", &[b"s/qr", b"g1", b"2-1"]),
        b":1\r\n".to_vec()
    );
    assert_eq!(rdb::lite::offset::dirty_len(&shared.lite.offsets), 1);
    flush_offsets(&shared);
    assert_eq!(rdb::lite::offset::dirty_len(&shared.lite.offsets), 0);
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qr", b"g1", b"0-0"]),
        b"-BUSYGROUP Consumer Group name already exists\r\n".to_vec(),
        "exactly one group record survives: the fresh one"
    );
}
