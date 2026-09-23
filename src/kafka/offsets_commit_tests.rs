//! Unit tests for `kafka::offsets_commit`: the v0/v1/v2 request
//! shapes, the generation rule (older generation -> ILLEGAL_GENERATION,
//! v0 preserves the stored fencing fields), offset round-trip through
//! the ledger, negative-offset rejection, and the OffsetFetch v0/v7
//! bodies (incl. the null-topics "all commits" walk).

use rocksdb::WriteBatch;

use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_i32, put_i64, put_nullable_string, put_string, Reader,
};
use crate::kafka::offsets_commit::{handle_offset_commit, handle_offset_fetch};
use crate::lite::model;
use crate::state::testutil;
use crate::state::Shared;
use crate::store::ops;

/// Seed a live partition-0 stream (1 entry, len 1).
fn seed(shared: &Shared, topic: &[u8]) {
    let stream = [topic, b"/q0"].concat();
    let prefix = crate::hash::slot_with_prefix(topic).1;
    let mut wb = WriteBatch::default();
    wb.put(
        model::entry_key(&prefix, &stream, model::EntryId { ms: 5, seq: 0 }),
        model::encode_entry(&[(b"v".as_slice(), b"x".as_slice())]),
    );
    wb.put(
        model::meta_key(&prefix, &stream),
        model::encode_meta_at(
            &model::MetaPayload {
                created_ms: 1,
                len: 1,
                last_ms: 5,
                ..Default::default()
            },
            0,
        ),
    );
    ops::batch_write(&shared.store, wb).unwrap();
}

/// OffsetCommit request body. v0: group+topics; v1: +generation/member;
/// v2: +retention.
fn commit_req(
    version: i16,
    group: &str,
    generation: i32,
    member: &str,
    topic: &str,
    partition: i32,
    offset: i64,
) -> Vec<u8> {
    let mut r = Vec::new();
    put_string(&mut r, group);
    if version >= 1 {
        put_i32(&mut r, generation);
        put_string(&mut r, member);
    }
    if version >= 2 {
        put_i64(&mut r, -1); // retention_time
    }
    put_array_len(&mut r, 1);
    put_string(&mut r, topic);
    put_array_len(&mut r, 1);
    put_i32(&mut r, partition);
    put_i64(&mut r, offset);
    put_nullable_string(&mut r, None); // metadata
    r
}

/// One (partition, error) row of the first topic of a commit response.
fn commit_row(body: &[u8]) -> (i32, i16) {
    let mut r = Reader::new(body);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    (r.i32().unwrap(), r.i16().unwrap())
}

/// OffsetFetch request body for explicit topics (or None topics).
fn fetch_req(version: i16, group: &str, topic: Option<&str>) -> Vec<u8> {
    use crate::kafka::frame as f;
    let flex = version >= 6;
    let mut r = Vec::new();
    if flex {
        f::put_compact_string(&mut r, group);
    } else {
        put_string(&mut r, group);
    }
    match topic {
        None => {
            if flex {
                r.push(0); // compact null array
            } else {
                put_i32(&mut r, -1);
            }
        }
        Some(t) => {
            if flex {
                f::put_compact_array_len(&mut r, 1);
                f::put_compact_string(&mut r, t);
                f::put_compact_array_len(&mut r, 1);
                put_i32(&mut r, 0);
                f::put_empty_tagged_fields(&mut r); // topic tags
            } else {
                put_array_len(&mut r, 1);
                put_string(&mut r, t);
                put_array_len(&mut r, 1);
                put_i32(&mut r, 0);
            }
        }
    }
    if version >= 7 {
        r.push(0); // require_stable
    }
    if flex {
        f::put_empty_tagged_fields(&mut r); // message tags
    }
    r
}

#[tokio::test]
async fn commit_then_fetch_roundtrip_v2_v0_v7() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ct");
    // v2 commit at offset 1 (generation 5, member m-1).
    let req = commit_req(2, "g1", 5, "m-1", "ct", 0, 1);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        2,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE));

    // v7 fetch (flexible: throttle + leader epoch + compact strings).
    let req = fetch_req(7, "g1", Some("ct"));
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 7, &sh).unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.i32(), Some(0), "throttle");
    assert_eq!(r.compact_array_len(), Some(Some(1)));
    r.compact_string();
    assert_eq!(r.compact_array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(1), "committed offset as stored");
    assert_eq!(r.i32(), Some(-1), "committed_leader_epoch");
    assert_eq!(r.compact_nullable_string(), Some(None), "metadata null");
    assert_eq!(r.i16(), Some(0));
    assert_eq!(
        r.remaining(),
        5,
        "partition+topic tags, error, message tags"
    );
    assert_eq!(
        &body[body.len() - 3..],
        &[0, 0, 0],
        "top error + message tag"
    );

    // v0 fetch: no throttle, no leader epoch.
    let req = fetch_req(0, "g1", Some("ct"));
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    r.array_len();
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(1));
    assert_eq!(r.nullable_string(), Some(None));
    assert_eq!(r.i16(), Some(0));

    // Null topics: every commit of the group comes back.
    let req = fetch_req(0, "g1", None);
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().unwrap(), "ct");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0));
    assert_eq!(r.i64(), Some(1));

    // Never-committed group: offset -1, error NONE.
    let req = fetch_req(0, "other", Some("ct"));
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.array_len();
    r.string();
    r.array_len();
    r.i32();
    assert_eq!(r.i64(), Some(-1));
    assert_eq!(r.nullable_string(), Some(None));
    assert_eq!(r.i16(), Some(0));
    // Unknown partition: error 3 + -1.
    let req = fetch_req(0, "g1", Some("missing"));
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.array_len();
    r.string();
    r.array_len();
    r.i32();
    assert_eq!(r.i64(), Some(-1));
    assert_eq!(r.nullable_string(), Some(None));
    assert_eq!(r.i16(), Some(errors::UNKNOWN_TOPIC_OR_PARTITION));
}

#[tokio::test]
async fn generation_rule_and_v0_preserves_fencing() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ct");
    // Commit at generation 5.
    let req = commit_req(2, "g1", 5, "m-1", "ct", 0, 1);
    let mut r = Reader::new(&req);
    handle_offset_commit(
        &mut r,
        2,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    // Older generation: ILLEGAL_GENERATION, nothing changes.
    let req = commit_req(2, "g1", 3, "m-2", "ct", 0, 2);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        2,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::ILLEGAL_GENERATION));
    let req = fetch_req(0, "g1", Some("ct"));
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 0, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.array_len();
    r.string();
    r.array_len();
    r.i32();
    assert_eq!(r.i64(), Some(1), "rejected commit did not move the offset");

    // Equal generation commits fine.
    let req = commit_req(1, "g1", 5, "m-2", "ct", 0, 2);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        1,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE));

    // v0 has no generation field: the commit lands and PRESERVES the
    // stored generation/leader.
    let req = commit_req(0, "g1", -1, "", "ct", 0, 3);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        0,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE));
    let row = crate::kafka::ledger::load(
        &sh.store,
        &crate::hash::slot_with_prefix(b"ct").1,
        b"ct/q0",
        b"g1",
    )
    .unwrap()
    .unwrap();
    assert_eq!(row.generation, 5, "v0 keeps the stored generation");
    assert_eq!(row.leader, "m-2", "v0 keeps the stored leader");
    assert_eq!(row.committed_ordinal, 3);
    // A v0 commit never trips ILLEGAL_GENERATION even against gen 5.
    let req = commit_req(0, "g1", -1, "", "ct", 0, 4);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        0,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE));
}

#[tokio::test]
async fn commit_rejects_negative_and_missing_partition() {
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ct");
    let req = commit_req(2, "g1", 1, "m", "ct", 0, -3);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        2,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::OFFSET_OUT_OF_RANGE));
    // Commit past the log end IS accepted (stored; Fetch will 1 later).
    let req = commit_req(2, "g1", 1, "m", "ct", 0, 99);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        2,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE));
    let req = commit_req(2, "g1", 1, "m", "ct", 7, 0);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(
        &mut r,
        2,
        &sh,
        &crate::kafka::coordinator::CoordRuntime::new(),
    )
    .await
    .unwrap();
    assert_eq!(commit_row(&body), (7, errors::UNKNOWN_TOPIC_OR_PARTITION));
}

/// Fence layer 1 (the coordinator runtime): an active group rejects
/// unenrolled members and stale generations BEFORE the ledger is read;
/// a group absent from the runtime (or Empty) keeps the P2 degraded
/// ledger-only path.
#[tokio::test]
async fn commit_fence_layers() {
    use crate::kafka::coordinator::{self, state};
    let sh = testutil::shared_with(testutil::test_config());
    seed(&sh, b"ct");
    let rt = coordinator::CoordRuntime::new();
    // Build an active group at generation 1 with member m1.
    {
        let a = state::JoinArgs {
            member_id: "m1",
            instance_id: None,
            client_id: "c",
            client_host: "h",
            session_timeout_ms: 100_000,
            rebalance_timeout_ms: 400_000,
            protocol_type: "consumer",
            protocol_name: "range",
            metadata: b"",
            now_ms: 0,
        };
        let st = state::join(state::new_group("consumer"), &a).0;
        rt.groups.write().unwrap().insert("g1".into(), st);
    }
    // Enrolled member, current generation: lands in the ledger.
    let req = commit_req(2, "g1", 1, "m1", "ct", 0, 5);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(&mut r, 2, &sh, &rt).await.unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE));
    // Stale generation: ILLEGAL_GENERATION, ledger untouched.
    let req = commit_req(2, "g1", 0, "m1", "ct", 0, 6);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(&mut r, 2, &sh, &rt).await.unwrap();
    assert_eq!(commit_row(&body), (0, errors::ILLEGAL_GENERATION));
    // Zombie member (not enrolled): UNKNOWN_MEMBER_ID.
    let req = commit_req(2, "g1", 1, "ghost", "ct", 0, 6);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(&mut r, 2, &sh, &rt).await.unwrap();
    assert_eq!(commit_row(&body), (0, errors::UNKNOWN_MEMBER_ID));
    // The ledger kept offset 5 from the authorized commit only.
    let req = fetch_req(7, "g1", Some("ct"));
    let mut r = Reader::new(&req);
    let body = handle_offset_fetch(&mut r, 7, &sh).unwrap();
    let mut r = Reader::new(&body);
    r.i32(); // throttle
    r.compact_array_len();
    r.compact_string();
    r.compact_array_len();
    r.i32();
    assert_eq!(r.i64(), Some(5));
    // Group unknown to the runtime (restart wiped membership): the
    // degraded path still fences a generation OLDER than the ledger's.
    let req = commit_req(2, "g1", 0, "m1", "ct", 0, 1);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(&mut r, 2, &sh, &coordinator::CoordRuntime::new())
        .await
        .unwrap();
    assert_eq!(commit_row(&body), (0, errors::ILLEGAL_GENERATION));
    let req = commit_req(2, "g1", 1, "m1", "ct", 0, 7);
    let mut r = Reader::new(&req);
    let body = handle_offset_commit(&mut r, 2, &sh, &coordinator::CoordRuntime::new())
        .await
        .unwrap();
    assert_eq!(commit_row(&body), (0, errors::NONE), "same gen re-commits");
}
