//! Multi-store scenario #110 -- the consumer-fleet control room on
//! the Kafka face (Api keys 3/2/11/14/15/8/9, scenario-zero until
//! now): RESP XADD seeds a topic and Produce v3 appends on top,
//! Metadata v1 answers Some(1) partition for the live topic and None
//! for an unknown name, ListOffsets v5 reads latest (hwm) vs earliest
//! (0), the two-phase JoinGroup/SyncGroup handshake (empty member ->
//! 79 -> rejoin -> generation 1) lands a Stable single-member group
//! that DescribeGroups v0 reports, and OffsetCommit v2 / OffsetFetch
//! v0 round-trip the fleet's committed offset. One REAL node, RESP +
//! kafka fronts.

mod common;
mod kafka_front_common;
mod scenario_common;

use kafka_front_common::groups::{commit_v2, describe_v0, fetch_offset_v0, join_v1, sync_v1};
use kafka_front_common::topics::{list_offsets_v5, metadata_partitions};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{front_world, r, FrontSpec};
use tokio::net::TcpStream;

const SESSION_MS: i32 = 30_000;

#[tokio::test]
async fn consumer_fleet_metadata_offsets_and_a_stable_group() {
    let spec = FrontSpec {
        kafka: true,
        ..Default::default()
    };
    let (front, a) = front_world("fleet110", &spec).await;
    let kafka = front.kafka.clone();

    // a) Seed the topic on the RESP plane (bare parent XADD -> the
    //    implicit q0 queue), then Produce v3 one record on top: the
    //    base offset echoes the pre-append length (1 record -> 2 hwm).
    let seed = r(&a, &[b"XADD", b"fleet", b"*", b"k", b"seed-entry"]).await;
    // bare-parent XADD answers the *2 id pair (ms + seq).
    assert!(seed.starts_with(b"*2"), "xadd seed: {seed:?}");
    let record = rdb::kafka::record::BatchRecord {
        timestamp_delta: 0,
        key: Some(b"fleet"),
        value: Some(b"telemetry-event"),
        headers: vec![],
    };
    let base = kafka_produce_v3(&kafka, 1, "fleet", &[record]).await;
    assert_eq!(base, 1, "base offset = pre-append len");

    // b) Metadata v1: the live topic owns its single partition; an
    //    unknown name answers None (topic-level error).
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka front");
    assert_eq!(metadata_partitions(&mut sock, 2, "fleet").await, Some(1), "live topic");
    assert_eq!(metadata_partitions(&mut sock, 3, "no-such-topic").await, None, "unknown topic");

    // c) ListOffsets v5: latest = hwm 2, earliest = 0; leader epoch
    //    rides the -1 sentinel.
    assert_eq!(list_offsets_v5(&mut sock, 4, "fleet", 0, -1).await, (0, -1, 2, -1), "latest = hwm");
    assert_eq!(list_offsets_v5(&mut sock, 5, "fleet", 0, -2).await, (0, -1, 0, -1), "earliest = 0");

    // d) The group handshake: an EMPTY member id answers 79 + a fresh
    //    broker-assigned id; the rejoin lands generation 1 with the
    //    member as leader holding the single subscription row.
    let j = join_v1(&mut sock, 6, "fleetops", SESSION_MS, 60_000, "", b"sub[fleet]").await;
    assert_eq!(j.error, 79, "member id required: {:?}", j.member_id);
    assert!(j.member_id.starts_with("rdb-fleetops-"), "assigned id: {}", j.member_id);
    let id = j.member_id.clone();
    let j = join_v1(&mut sock, 7, "fleetops", SESSION_MS, 60_000, &id, b"sub[fleet]").await;
    assert_eq!(j.error, 0, "rejoin clean");
    assert_eq!(j.generation, 1, "solo barrier generation");
    assert_eq!(j.leader, id, "the joiner is the leader");
    assert_eq!(j.protocol_name.as_deref(), Some("range"));
    assert_eq!(j.members.len(), 1, "leader sees its own row");
    assert_eq!(j.members[0].member_id, id);

    // e) SyncGroup distributes the single slice; DescribeGroups v0
    //    reports Stable + the member.
    let (err, assignment) = sync_v1(&mut sock, 8, "fleetops", &id, 1, &[(id.as_str(), b"[fleetp0]")]).await;
    assert_eq!(err, 0, "sync clean");
    assert_eq!(assignment, b"[fleetp0]");
    let d = describe_v0(&mut sock, 9, "fleetops").await;
    assert_eq!(d.state, "Stable");
    assert_eq!(d.protocol_type, "consumer");
    assert_eq!(d.protocol.as_deref(), Some("range"));
    assert_eq!(d.member_ids, vec![id.clone()], "the single member");

    // f) Commit the fleet's offset at hwm and read it back.
    assert_eq!(commit_v2(&mut sock, 10, "fleetops", 1, &id, "fleet", 0, 2).await, 0, "commit clean");
    assert_eq!(fetch_offset_v0(&mut sock, 11, "fleetops", "fleet", 0).await, 2, "committed offset echoes");
}
