//! P3 consumer-group coordinator against the REAL binary: the full
//! eager-rebalance conversation between two connections -- the
//! two-phase member-id handshake, the join barrier, leader-only
//! assignment distribution, DescribeGroups snapshots, heartbeats,
//! LeaveGroup -> REBALANCE_IN_PROGRESS -> rejoin at a new generation,
//! and the commit-fencing lattice (22 stale generation / 25 unknown
//! member / authorized commits landing in the durable ledger).

mod common;
mod kafka_front_common;

use kafka_front_common::groups::{
    commit_v2, describe_v0, fetch_offset_v0, heartbeat_v0, join_v1, leave_v0, sync_v1,
};
use kafka_front_common::{resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::errors;
use tokio::net::TcpStream;

const SESSION_MS: i32 = 30_000;

/// Lay down the stream behind topic "t" partition 0 so commits have a
/// partition -> queue mapping to bind to.
async fn seed_stream(resp: &str) {
    resp_one_shot(resp, &[b"XADD", b"t/q0", b"*", b"k1", b"v1"]).await;
}

#[tokio::test]
async fn two_consumers_full_rebalance_conversation() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    seed_stream(&resp).await;
    wait_accepting(&kafka, &mut node, "kafka").await;

    let mut m1 = TcpStream::connect(&kafka).await.expect("connect m1");
    let mut m2 = TcpStream::connect(&kafka).await.expect("connect m2");

    // Two-phase handshake (KIP-394): an EMPTY member id answers
    // MEMBER_ID_REQUIRED(79) + a fresh id (25 would make clients drop
    // the id and retry forever).
    let r = join_v1(&mut m1, 1, "g1", SESSION_MS, 60_000, "", b"sub[t0]").await;
    assert_eq!(r.error, errors::MEMBER_ID_REQUIRED);
    assert!(r.member_id.starts_with("rdb-g1-"));
    let id1 = r.member_id.clone();
    let r = join_v1(&mut m2, 2, "g1", SESSION_MS, 60_000, "", b"sub[t0]").await;
    assert_eq!(r.error, errors::MEMBER_ID_REQUIRED);
    let id2 = r.member_id.clone();
    assert_ne!(id1, id2, "fresh ids are unique");

    // m1 joins with its id: solo barrier -> generation 1, m1 is the
    // leader and sees its own subscription row.
    let r = join_v1(&mut m1, 3, "g1", SESSION_MS, 60_000, &id1, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 1);
    assert_eq!(r.leader, id1);
    assert_eq!(r.protocol_name.as_deref(), Some("range"));
    assert_eq!(r.members.len(), 1);
    assert_eq!(r.members[0].member_id, id1);
    assert_eq!(r.members[0].metadata, b"sub[t0]");

    // m2's join kicks the rebalance and PARKS until m1 rejoins.
    let id2_for_task = id2.clone();
    let parked = tokio::spawn(async move {
        let r = join_v1(
            &mut m2,
            4,
            "g1",
            SESSION_MS,
            60_000,
            &id2_for_task,
            b"sub[t0]",
        )
        .await;
        (r, m2)
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let r1 = join_v1(&mut m1, 5, "g1", SESSION_MS, 60_000, &id1, b"sub[t0]").await;
    let (r2, m2) = parked.await.unwrap();
    let mut m2 = m2;
    assert_eq!(r1.error, errors::NONE);
    assert_eq!(r2.error, errors::NONE);
    assert_eq!(r1.generation, 2, "kick bumped the generation");
    assert_eq!(r2.generation, r1.generation, "shared barrier");
    assert_eq!(r1.leader, id1, "oldest joiner stays leader");
    assert_eq!(r1.members.len(), 2, "leader sees both rows");
    assert_eq!(r2.members.len(), 0, "follower list is empty");

    // Leader distributes; both Stable replies carry their own slice.
    let (err, a) = sync_v1(
        &mut m1,
        6,
        "g1",
        &id1,
        2,
        &[(id1.as_str(), b"[t0p0]"), (id2.as_str(), b"[t0p1]")],
    )
    .await;
    assert_eq!(err, errors::NONE);
    assert_eq!(a, b"[t0p0]");
    let (err, a) = sync_v1(&mut m2, 7, "g1", &id2, 2, &[]).await;
    assert_eq!(err, errors::NONE);
    assert_eq!(a, b"[t0p1]");

    // DescribeGroups sees Stable with both members and the protocol.
    let d = describe_v0(&mut m1, 8, "g1").await;
    assert_eq!(d.state, "Stable");
    assert_eq!(d.protocol_type, "consumer");
    assert_eq!(d.protocol.as_deref(), Some("range"));
    assert_eq!(d.member_ids.len(), 2);
    assert!(d.member_ids.contains(&id1));
    assert!(d.member_ids.contains(&id2));

    // Heartbeats keep both sessions alive.
    assert_eq!(heartbeat_v0(&mut m1, 9, "g1", 2, &id1).await, errors::NONE);
    assert_eq!(heartbeat_v0(&mut m2, 10, "g1", 2, &id2).await, errors::NONE);

    // An enrolled member at the current generation commits durably.
    assert_eq!(
        commit_v2(&mut m1, 11, "g1", 2, &id1, "t", 0, 5).await,
        errors::NONE
    );
    assert_eq!(fetch_offset_v0(&mut m1, 12, "g1", "t", 0).await, 5);

    // Scenario 2: m2 leaves -> the group rebalances; m1's next
    // heartbeat answers REBALANCE_IN_PROGRESS and its rejoin (the
    // client-native reaction) lands generation 3 alone.
    assert_eq!(leave_v0(&mut m2, 13, "g1", &id2).await, errors::NONE);
    assert_eq!(
        heartbeat_v0(&mut m1, 14, "g1", 2, &id1).await,
        errors::REBALANCE_IN_PROGRESS
    );
    let d = describe_v0(&mut m1, 15, "g1").await;
    assert_eq!(d.state, "PreparingRebalance");
    let r = join_v1(&mut m1, 16, "g1", SESSION_MS, 60_000, &id1, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 3);
    assert_eq!(r.members.len(), 1, "m2 is gone");
    let (err, a) = sync_v1(&mut m1, 17, "g1", &id1, 3, &[(id1.as_str(), b"[t0p0]")]).await;
    assert_eq!((err, a.as_slice()), (errors::NONE, &b"[t0p0]"[..]));
    let d = describe_v0(&mut m1, 18, "g1").await;
    assert_eq!(d.state, "Stable");
    assert_eq!(d.member_ids, vec![id1.clone()]);

    // Scenario 4: the commit-fencing lattice around the live group.
    assert_eq!(
        commit_v2(&mut m1, 19, "g1", 2, &id1, "t", 0, 9).await,
        errors::ILLEGAL_GENERATION,
        "stale generation"
    );
    assert_eq!(
        commit_v2(&mut m1, 20, "g1", 3, "ghost", "t", 0, 9).await,
        errors::UNKNOWN_MEMBER_ID,
        "zombie member"
    );
    // The leaver fails the MEMBERSHIP check first (the broker checks
    // the member before the generation).
    assert_eq!(
        commit_v2(&mut m2, 21, "g1", 2, &id2, "t", 0, 9).await,
        errors::UNKNOWN_MEMBER_ID,
        "the leaver is fenced"
    );
    assert_eq!(
        fetch_offset_v0(&mut m1, 22, "g1", "t", 0).await,
        5,
        "fenced commits wrote nothing"
    );
    assert_eq!(
        commit_v2(&mut m1, 23, "g1", 3, &id1, "t", 0, 6).await,
        errors::NONE
    );
    assert_eq!(fetch_offset_v0(&mut m1, 24, "g1", "t", 0).await, 6);
}
