//! FLUSHDB x Lite-stream e2e: the wipe must leave the lite control plane
//! CONSISTENT with the wiped data plane. Pins the contract triad:
//! (1) no orphan group record is resurrected by the offset flusher after
//!     the wipe (regression: the in-memory dirty offset cache used to
//!     outlive FLUSHDB and re-write a group record onto the wiped
//!     keyspace on the next 200ms flush round);
//! (2) a stale (wiped) group reads NOGROUP, matching Redis semantics;
//! (3) a wiped stream rebuilds cleanly: auto ids never regress or
//!     collide with pre-wipe ids, and new groups deliver end-to-end.

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

/// Extract the `<ms>-<seq>` id out of a bulk-string frame like
/// `$15\r\n1788625033831-0\r\n` and compare it as (ms, seq).
fn id_parts(frame: &str) -> (u64, u64) {
    let id = frame
        .split(|c: char| !(c.is_ascii_digit() || c == '-'))
        .find(|s| s.contains('-') && s.len() > 2)
        .expect("an id inside the reply frame");
    let (ms, seq) = id.split_once('-').expect("ms-seq");
    (ms.parse().expect("ms"), seq.parse().expect("seq"))
}

#[test]
fn flushdb_wipes_streams_and_keeps_control_plane_consistent() {
    let shared = Arc::new(shared_at("43870").0);

    // ---- seed: two entries, group g1 delivers and acks both ----
    assert!(text(&call(&shared, "xadd", &[b"s/qb", b"1-1", b"f", b"v"])).contains("1-1"));
    assert!(text(&call(&shared, "xadd", &[b"s/qb", b"2-1", b"f", b"v"])).contains("2-1"));
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qb", b"g1", b"0-0"]),
        b"+OK\r\n".to_vec()
    );
    let delivered = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g1", b"c1", b"streams", b"s/qb", b">"],
    ));
    assert!(
        delivered.contains("1-1") && delivered.contains("2-1"),
        "{delivered}"
    );
    assert_eq!(
        call(&shared, "xack", &[b"s/qb", b"g1", b"1-1", b"2-1"]),
        b":2\r\n".to_vec()
    );
    // The ack left g1 dirty in the offset cache -- the resurrection seed.
    assert_eq!(rdb::lite::offset::dirty_len(&shared.lite.offsets), 1);
    assert!(text(&call(&shared, "xinfo", &[b"groups", b"s/qb"])).contains("g1"));

    // ---- wipe ----
    assert_eq!(call(&shared, "flushdb", &[]), b"+OK\r\n".to_vec());
    assert_eq!(call(&shared, "dbsize", &[]), b":0\r\n".to_vec());
    assert_eq!(call(&shared, "xlen", &[b"s/qb"]), b":0\r\n".to_vec());
    let groups = text(&call(&shared, "xinfo", &[b"groups", b"s/qb"]));
    assert!(
        !groups.contains("g1"),
        "on-disk group record survived: {groups}"
    );

    // ---- (1) the offset flush round must not resurrect g1 ----
    flush_offsets(&shared);
    assert_eq!(rdb::lite::offset::dirty_len(&shared.lite.offsets), 0);
    let groups = text(&call(&shared, "xinfo", &[b"groups", b"s/qb"]));
    assert!(
        !groups.contains("g1"),
        "offset flusher resurrected an orphan group after FLUSHDB: {groups}"
    );

    // ---- (2) the stale group reads NOGROUP, not empty deliveries ----
    let stale = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g1", b"c1", b"streams", b"s/qb", b">"],
    ));
    assert!(stale.starts_with("-NOGROUP"), "{stale}");

    // ---- (3) rebuild: ids do not regress, new groups deliver ----
    let rebuilt = text(&call(&shared, "xadd", &[b"s/qb", b"*", b"f", b"v"]));
    assert!(id_parts(&rebuilt) > (2, 1), "auto id regressed: {rebuilt}");
    assert_eq!(call(&shared, "xlen", &[b"s/qb"]), b":1\r\n".to_vec());
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qb", b"g2", b"$"]),
        b"+OK\r\n".to_vec()
    );
    // Recreating the SAME name also works: nothing survived the wipe.
    assert_eq!(
        call(&shared, "xgroup", &[b"create", b"s/qb", b"g1", b"$"]),
        b"+OK\r\n".to_vec()
    );
    let after = text(&call(&shared, "xadd", &[b"s/qb", b"*", b"f", b"v2"]));
    assert!(
        id_parts(&after) > id_parts(&rebuilt),
        "{after} vs {rebuilt}"
    );
    let got = text(&call(
        &shared,
        "xreadgroup",
        &[b"group", b"g2", b"c2", b"streams", b"s/qb", b">"],
    ));
    assert!(
        got.contains(&after_id(&after)),
        "g2 missed the new entry: {got}"
    );
    let id = after_id(&after);
    assert_eq!(
        call(&shared, "xack", &[b"s/qb", b"g2", id.as_bytes()]),
        b":1\r\n".to_vec()
    );

    // ---- a second wipe after the rebuild is equally clean ----
    assert_eq!(call(&shared, "flushdb", &[]), b"+OK\r\n".to_vec());
    flush_offsets(&shared);
    let groups = text(&call(&shared, "xinfo", &[b"groups", b"s/qb"]));
    assert!(!groups.contains("g1") && !groups.contains("g2"), "{groups}");
}

/// The bare `<ms>-<seq>` id string from an XADD reply frame.
fn after_id(frame: &str) -> String {
    let (ms, seq) = id_parts(frame);
    format!("{ms}-{seq}")
}
