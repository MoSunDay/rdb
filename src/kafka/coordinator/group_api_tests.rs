//! Wire roundtrips for every advertised version of JoinGroup (v0-v4)
//! and SyncGroup (v0-v4), plus the async barrier behaviors: the
//! two-phase member-id handshake, the leader-only member list, the
//! join barrier waking parked followers, and the sync handoff.

use std::sync::Arc;

use super::group_api;
use super::CoordRuntime;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bytes, put_compact_array_len, put_compact_bytes, put_compact_string,
    put_i32, put_nullable_string, put_string, Reader,
};

/// JoinGroup request body per the version ladder (group, session,
/// [rebalance], member, [v4 instance], protocol_type, protocols).
fn join_req(
    version: i16,
    group: &str,
    member: &str,
    instance: Option<&str>,
    protocols: &[(&str, &[u8])],
) -> Vec<u8> {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, 60_000); // session_timeout_ms
    if version >= 1 {
        put_i32(&mut b, 300_000); // rebalance_timeout_ms
    }
    put_string(&mut b, member);
    if version >= 5 {
        put_nullable_string(&mut b, instance);
    }
    put_string(&mut b, "consumer");
    put_array_len(&mut b, protocols.len());
    for (name, md) in protocols {
        put_string(&mut b, name);
        put_bytes(&mut b, md);
    }
    b
}

#[derive(Debug)]
struct JoinOut {
    error: i16,
    generation: i32,
    protocol_type: Option<String>,
    protocol_name: Option<String>,
    leader: String,
    member_id: String,
    members: Vec<(String, Option<String>, Vec<u8>)>,
}

fn parse_join(body: &[u8], version: i16) -> JoinOut {
    let mut r = Reader::new(body);
    if version >= 2 {
        assert_eq!(r.i32(), Some(0), "throttle first (v2+)");
    }
    let error = r.i16().unwrap();
    let generation = r.i32().unwrap();
    let protocol_type = if version >= 7 { r.nullable_string().unwrap() } else { None };
    let protocol_name = if version >= 1 {
        r.nullable_string().unwrap()
    } else {
        Some(r.string().unwrap())
    };
    let leader = r.string().unwrap();
    let member_id = r.string().unwrap();
    let n = r.array_len().unwrap().unwrap_or(0);
    let mut members = Vec::new();
    for _ in 0..n {
        let id = r.string().unwrap();
        let inst = if version >= 5 { r.nullable_string().unwrap() } else { None };
        let md = r.bytes().unwrap().unwrap_or(&[]).to_vec();
        members.push((id, inst, md));
    }
    assert_eq!(r.remaining(), 0, "no trailing bytes");
    JoinOut { error, generation, protocol_type, protocol_name, leader, member_id, members }
}

async fn round_join(
    rt: &Arc<CoordRuntime>,
    version: i16,
    group: &str,
    member: &str,
    instance: Option<&str>,
) -> JoinOut {
    let body = join_req(version, group, member, instance, &[("range", b"sub[tp]")]);
    let mut r = Reader::new(&body);
    let sh = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    let out = group_api::handle_join_group(&mut r, version, rt, &sh, Some("client-1"), "10.1.1.1")
        .await
        .unwrap();
    parse_join(&out, version)
}

#[tokio::test]
async fn join_group_roundtrips_all_versions() {
    for version in 0..=4 {
        let rt = Arc::new(CoordRuntime::new());
        // First join with an EMPTY member id: v1+ answers 79
        // MEMBER_ID_REQUIRED + the generated id (KIP-394 two-phase);
        // v0 assigns it and proceeds.
        let out = round_join(&rt, version, "g1", "", None).await;
        assert!(out.member_id.starts_with("rdb-g1-"), "carries a fresh id");
        let id = out.member_id.clone();
        if version >= 1 {
            assert_eq!(out.error, errors::MEMBER_ID_REQUIRED);
            assert_eq!(out.members.len(), 0);
            // The handshake's second leg creates the group; the solo
            // member IS the leader and sees itself in the list.
            let out = round_join(&rt, version, "g1", &id, None).await;
            assert_eq!(out.error, errors::NONE);
            assert_eq!(out.generation, 1);
            assert_eq!(out.leader, id);
            assert_eq!(out.member_id, id);
            assert_eq!(out.protocol_name.as_deref(), Some("range"));
            // protocol_type joins the JoinGroup reply at v7 (above cap).
            assert_eq!(out.members.len(), 1, "leader sees the member list");
            assert_eq!(out.members[0].0, id);
            assert_eq!(out.members[0].2, b"sub[tp]");
            // member-row instance joins at v5 (above cap).
        } else {
            assert_eq!(out.error, errors::NONE, "v0 assigns silently");
            assert_eq!(out.generation, 1);
            assert_eq!(out.leader, id);
            assert_eq!(out.members.len(), 1, "v0 leader sees itself");
        }
        // A second member kicks a rebalance: its join PARKS until m1
        // rejoins (one task per connection), then both replies share
        // generation 2 with the member list only on the leader's.
        let rt2 = Arc::clone(&rt);
        let m2 = tokio::spawn(async move { round_join(&rt2, version, "g1", "m2", None).await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let out1 = round_join(&rt, version, "g1", &id, None).await;
        let out2 = m2.await.unwrap();
        assert_eq!(out1.error, errors::NONE);
        assert_eq!(out2.error, errors::NONE);
        assert_eq!(out1.generation, 2, "kick bumped the generation");
        assert_eq!(out2.generation, out1.generation, "shared barrier");
        assert_eq!(out1.leader, id, "oldest joiner stays leader");
        assert_eq!(out2.leader, id);
        assert_eq!(out1.members.len(), 2, "leader sees both");
        assert_eq!(out2.members.len(), 0, "follower list is empty");
    }
}

#[tokio::test]
async fn join_instance_id_fencing() {
    // group_instance_id rides the wire from v5 (classic framing until
    // v6): exercised handler-level even though dispatch caps at v4.
    let rt = Arc::new(CoordRuntime::new());
    let out = round_join(&rt, 5, "g1", "", Some("inst-1")).await;
    let id = out.member_id.clone();
    let out = round_join(&rt, 5, "g1", &id, Some("inst-1")).await;
    assert_eq!(out.error, errors::NONE);
    // Same instance id under a DIFFERENT member id: fenced.
    let out = round_join(&rt, 5, "g1", "other", Some("inst-1")).await;
    assert_eq!(out.error, errors::FENCED_INSTANCE_ID);
}

/// SyncGroup request body: classic (v0-v3) or flexible (v4+).
fn sync_req(version: i16, group: &str, member: &str, gen: i32, assigns: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = Vec::new();
    if version >= 4 {
        put_compact_string(&mut b, group);
        put_i32(&mut b, gen);
        put_compact_string(&mut b, member);
        crate::kafka::frame::put_compact_nullable_string(&mut b, None);
        put_compact_array_len(&mut b, assigns.len());
        for (id, a) in assigns {
            put_compact_string(&mut b, id);
            put_compact_bytes(&mut b, a);
        }
        crate::kafka::frame::put_empty_tagged_fields(&mut b);
    } else {
        put_string(&mut b, group);
        put_i32(&mut b, gen);
        put_string(&mut b, member);
        if version >= 3 {
            crate::kafka::frame::put_nullable_string(&mut b, None); // instance id
        }
        put_array_len(&mut b, assigns.len());
        for (id, a) in assigns {
            put_string(&mut b, id);
            put_bytes(&mut b, a);
        }
    }
    b
}

async fn round_sync(
    rt: &Arc<CoordRuntime>,
    version: i16,
    group: &str,
    member: &str,
    gen: i32,
    assigns: &[(&str, &[u8])],
) -> (i16, Vec<u8>) {
    let body = sync_req(version, group, member, gen, assigns);
    let mut r = Reader::new(&body);
    let out = group_api::handle_sync_group(&mut r, version, rt).await.unwrap();
    let mut r = Reader::new(&out);
    if version >= 1 {
        assert_eq!(r.i32(), Some(0), "throttle first (v1+)");
    }
    let err = r.i16().unwrap();
    let assignment = r.bytes().unwrap().unwrap_or(&[]).to_vec();
    if version >= 4 {
        assert_eq!(r.remaining(), 1, "flexible tagged tail");
    } else {
        assert_eq!(r.remaining(), 0);
    }
    (err, assignment)
}

#[tokio::test]
async fn sync_group_roundtrips_all_versions() {
    for version in 0..=4 {
        let rt = Arc::new(CoordRuntime::new());
        // Solo member through its barrier (v1+ skips the handshake
        // because the id is already non-empty).
        let out = round_join(&rt, version, "g1", "m1", None).await;
        assert_eq!(out.error, errors::NONE);
        let leader = out.member_id.clone();
        let gen1 = out.generation;
        // m2 joins (kick, parks) and m1 rejoins: generation 2 for both.
        let rt2 = Arc::clone(&rt);
        let m2join = tokio::spawn(async move { round_join(&rt2, version, "g1", "m2", None).await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let out1 = round_join(&rt, version, "g1", &leader, None).await;
        let out2 = m2join.await.unwrap();
        assert_eq!(out1.generation, gen1 + 1);
        assert_eq!(out2.generation, out1.generation);
        let gen2 = out1.generation;
        // Leader distributes: gets its own assignment back.
        let assigns = [("m1", b"a-m1".as_ref()), ("m2", b"a-m2".as_ref())];
        let (err, a) = round_sync(&rt, version, "g1", &leader, gen2, &assigns).await;
        assert_eq!(err, errors::NONE);
        assert_eq!(a, b"a-m1", "leader gets its own assignment back");
        // Follower sync after Stable: idempotent, returns its slice.
        let (err, a) = round_sync(&rt, version, "g1", "m2", gen2, &[]).await;
        assert_eq!(err, errors::NONE);
        assert_eq!(a, b"a-m2");
        // Error lattice: stale generation 22, unknown member/group 25.
        let (err, _) = round_sync(&rt, version, "g1", "m1", gen1, &[]).await;
        assert_eq!(err, errors::ILLEGAL_GENERATION);
        let (err, _) = round_sync(&rt, version, "g1", "ghost", gen2, &[]).await;
        assert_eq!(err, errors::UNKNOWN_MEMBER_ID);
        let (err, _) = round_sync(&rt, version, "nope", "m1", gen2, &[]).await;
        assert_eq!(err, errors::UNKNOWN_MEMBER_ID);
    }
}

#[tokio::test]
async fn sync_handoff_wakes_parked_follower() {
    // Two members through one barrier (the follower's join parks until
    // the leader rejoins), then: the follower's SyncGroup parks in
    // CompletingSync; the leader's concurrent SyncGroup distributes and
    // the follower wakes with its slice -- one task per connection.
    let rt = Arc::new(CoordRuntime::new());
    let out = round_join(&rt, 1, "g1", "m1", None).await; // gen 1, CS
    let leader = out.member_id.clone();
    let rt2 = Arc::clone(&rt);
    let m2join = tokio::spawn(async move { round_join(&rt2, 1, "g1", "m2", None).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let out1 = round_join(&rt, 1, "g1", &leader, None).await;
    let out2 = m2join.await.unwrap();
    assert_eq!(out1.generation, out2.generation);
    let gen = out1.generation;
    let rt3 = Arc::clone(&rt);
    let follower = tokio::spawn(async move { round_sync(&rt3, 1, "g1", "m2", gen, &[]).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let (err, a) = round_sync(&rt, 1, "g1", &leader, gen, &[("m1", b"L"), ("m2", b"F")]).await;
    assert_eq!((err, a.as_slice()), (errors::NONE, &b"L"[..]));
    let (err, a) = follower.await.unwrap();
    assert_eq!((err, a.as_slice()), (errors::NONE, &b"F"[..]));
}

#[tokio::test]
async fn join_barrier_wakes_parked_follower() {
    // Two concurrent connection tasks: m1 joins and completes alone;
    // m2's join kicks a rebalance and parks; m1's rejoin completes the
    // barrier and m2's parked call wakes with the same generation.
    let rt = Arc::new(CoordRuntime::new());
    let out = round_join(&rt, 1, "g1", "m1", None).await;
    let gen1 = out.generation;
    let rt2 = Arc::clone(&rt);
    let m2 = tokio::spawn(async move { round_join(&rt2, 1, "g1", "m2", None).await });
    // Give the parked call a beat to actually park, then rejoin.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let out1 = round_join(&rt, 1, "g1", "m1", None).await;
    let out2 = m2.await.unwrap();
    assert_eq!(out1.generation, out2.generation);
    assert_eq!(out1.generation, gen1 + 1);
    assert_eq!(out1.leader, "m1");
    assert_eq!(out2.leader, "m1");
    assert_eq!(out1.members.len(), 2);
    assert_eq!(out2.members.len(), 0, "follower list empty");
}
