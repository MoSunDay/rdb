//! Multi-store scenario #168 -- a partition-assignment rehearsal: the group
//! leader receives every member's subscription in its JoinGroup reply, hands
//! the coordinator an opaque per-member assignment through SyncGroup, and each
//! follower -- whose SyncGroup was parked -- receives exactly its own bytes
//! back (unknown ids in the leader's list are ignored silently); the rehearsal
//! also pins the membership/generation error ladder (22/23/24/25), the
//! leader's-first-protocol rule, and the assigned fleet then reads its log.
//! SURFACES: Kafka wire + RESP stream. One REAL node, fronts kafka + resp.

use crate::kafka_front_common;
use crate::scenario_common;
use kafka_front_common::groups::{DescribeView, JoinView, describe_v0, heartbeat_v0, join_v1, sync_v1};
use kafka_front_common::topics::{create_topics, fetch_offset_v0_raw};
use kafka_front_common::{kafka_req, kafka_round};
use rdb::kafka::frame::{Reader, put_array_len, put_bytes, put_i32, put_i64, put_string};
use rdb::kafka::record::{BatchRecord, RecordBatch, parse_batch};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{FrontSpec, front_world, int_of, r};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const TAG: &str = "kassign";
const GROUP: &str = "assign168";
const GHOST_GROUP: &str = "ghostgroup";
const TOPIC: &str = "assign";
const STREAM: &str = "assign/p0";
const MEMBER_B: &str = "assign-b";
const MEMBER_C: &str = "assign-c";
const SUB_A: &[u8] = b"sub[assign-m1]\x00\x01";
const SUB_B: &[u8] = b"sub[assign-b]";
const SUB_C: &[u8] = b"sub[assign-c]";
const SLICE_LEADER: &[u8] = b"p0:leader";
const SLICE_FOLLOWER: &[u8] = b"p0:follower\x00\x01";
const SESSION_MS: i32 = 1_500;
const REBALANCE_MS: i32 = 60_000;
const API_FETCH: i16 = 1;
const API_JOIN_GROUP: i16 = 11;
// Kafka error codes pinned by this scenario (local consts).
const ERR_NONE: i16 = 0;
const ERR_ILLEGAL_GENERATION: i16 = 22;
const ERR_INCONSISTENT_GROUP_PROTOCOL: i16 = 23;
const ERR_INVALID_GROUP_ID: i16 = 24;
const ERR_UNKNOWN_MEMBER_ID: i16 = 25;
const ERR_REBALANCE_IN_PROGRESS: i16 = 27;
const ERR_MEMBER_ID_REQUIRED: i16 = 79;
/// What the assigned fleet reads at the end (checked on both faces).
const VALUES: [&str; 3] = ["assign-one", "assign-two", "assign-three"];

/// Next correlation id of one socket's counter.
fn next(corr: &mut i32) -> i32 {
    *corr += 1;
    *corr
}

/// Connect one kafka-front client (the listener may still be warming up).
async fn connect(addr: &str, what: &str) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(addr).await {
            Ok(s) => return s,
            Err(_) => {
                assert!(Instant::now() < deadline, "kafka front never accepted for {what}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Fetch v0 round for (topic, partition 0, offset): (error, hwm, batch).
async fn fetch_v0(sock: &mut TcpStream, corr: i32, topic: &str, offset: i64) -> (i16, i64, RecordBatch) {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    put_i32(&mut b, 0); // max_wait_ms
    put_i32(&mut b, 1); // min_bytes
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0); // partition
    put_i64(&mut b, offset);
    put_i32(&mut b, 1 << 20); // partition max_bytes
    let payload = kafka_round(sock, &kafka_req(API_FETCH, 0, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation id echo");
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)), "one topic");
    assert_eq!(r.string().as_deref(), Some(topic), "topic echoed");
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    assert_eq!(r.i32(), Some(0), "partition 0");
    let error = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    let n = r.i32().unwrap_or(-1) as usize;
    let records = r.take(n).unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0, "v0 reply fully drained");
    (error, hwm, parse_batch(&records).expect("CRC-verified record batch"))
}

/// The leader's member rows of a JoinGroup reply as (id, subscription bytes).
fn rows(view: &JoinView) -> Vec<(String, Vec<u8>)> {
    view.members
        .iter()
        .map(|m| (m.member_id.clone(), m.metadata.clone()))
        .collect()
}

/// JoinGroup v1 with an explicit protocol_type + protocol NAME (hand-framed,
/// one protocol): (error, generation, protocol_name, leader, member rows).
async fn join_v1_proto(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    member: &str,
    protocol_type: &str,
    protocol: &str,
    subscription: &[u8],
) -> (i16, i32, Option<String>, String, Vec<(String, Vec<u8>)>) {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, SESSION_MS);
    put_i32(&mut b, REBALANCE_MS);
    put_string(&mut b, member);
    put_string(&mut b, protocol_type);
    put_array_len(&mut b, 1);
    put_string(&mut b, protocol);
    put_bytes(&mut b, subscription);
    let payload = kafka_round(sock, &kafka_req(API_JOIN_GROUP, 1, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation id echo");
    let mut r = Reader::new(&payload[r.pos()..]);
    let error = r.i16().unwrap();
    let generation = r.i32().unwrap();
    let protocol_name = r.nullable_string().unwrap();
    let leader = r.string().unwrap();
    r.string(); // member_id echo
    let members = (0..r.array_len().unwrap().unwrap_or(0))
        .map(|_| (r.string().unwrap(), r.bytes().unwrap().unwrap_or(&[]).to_vec()))
        .collect();
    assert_eq!(r.remaining(), 0, "join reply fully consumed");
    (error, generation, protocol_name, leader, members)
}

/// Poll DescribeGroups every 50ms until `done` holds (5s deadline).
async fn poll_describe(
    sock: &mut TcpStream,
    corr: &mut i32,
    done: impl Fn(&DescribeView) -> bool,
    what: &str,
) -> DescribeView {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let d = describe_v0(sock, next(corr), GROUP).await;
        if done(&d) {
            return d;
        }
        assert!(
            Instant::now() < deadline,
            "DescribeGroups never reached {what}: state {} members {:?}",
            d.state,
            d.member_ids
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn kafka_leader_assignment() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        ..Default::default()
    };
    let (front, resp) = front_world(TAG, &spec).await;
    let ctx = front.node.ctx();
    let mut probe = connect(&front.kafka, "the observer").await;
    let mut cp = 0;
    assert_eq!(
        create_topics(&mut probe, next(&mut cp), TOPIC, 1, -1, false).await,
        ERR_NONE,
        "create {TOPIC}: {ctx}"
    );

    // 1) m1 joins through the two-phase handshake (generation 1); m2 arrives
    //    with a client-chosen id and parks until m1 rejoins -- generation 2,
    //    and the leader's roster carries BOTH subscriptions verbatim.
    let mut s1 = connect(&front.kafka, "m1").await;
    let mut c1 = 0;
    let hello = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, "", SUB_A).await;
    assert_eq!(hello.error, ERR_MEMBER_ID_REQUIRED, "empty member id");
    let m1 = hello.member_id.clone();
    let solo = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (solo.error, solo.generation, solo.leader.as_str()),
        (ERR_NONE, 1, m1.as_str()),
        "solo barrier: generation 1, m1 leads"
    );
    let addr = front.kafka.clone();
    let join_b = tokio::spawn(async move {
        let mut sb = connect(&addr, "m2").await;
        let view = join_v1(&mut sb, 1, GROUP, SESSION_MS, REBALANCE_MS, MEMBER_B, SUB_B).await;
        (sb, view)
    });
    poll_describe(&mut probe, &mut cp, |d| d.state == "PreparingRebalance" && d.member_ids.len() == 2, "m2's kick").await;
    assert_eq!(
        heartbeat_v0(&mut s1, next(&mut c1), GROUP, 1, &m1).await,
        ERR_REBALANCE_IN_PROGRESS,
        "m1 is told to rejoin"
    );
    let again = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (again.error, again.generation, again.leader.as_str()),
        (ERR_NONE, 2, m1.as_str()),
        "second barrier: generation 2, m1 leads"
    );
    assert_eq!(
        rows(&again),
        vec![(m1.clone(), SUB_A.to_vec()), (MEMBER_B.to_string(), SUB_B.to_vec())],
        "every member's subscription reaches the leader verbatim, in join order"
    );
    let (s2, view_b) = tokio::time::timeout(Duration::from_secs(5), join_b)
        .await
        .expect("m2's parked join returned within 5s")
        .expect("m2 join task alive");
    assert_eq!(
        (view_b.error, view_b.generation, view_b.leader.as_str(), view_b.members.len()),
        (ERR_NONE, 2, m1.as_str(), 0),
        "the follower's reply carries no roster"
    );

    // 2) The follower's SyncGroup goes FIRST and really parks (handshake:
    //    the task signals right before it writes); the leader then hands the
    //    coordinator opaque slices -- an unknown id is ignored, the follower
    //    gets exactly its own bytes.
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let mut s2 = s2;
    let mut sync_b = tokio::spawn(async move {
        ready_tx.send(()).expect("test alive");
        let r = sync_v1(&mut s2, 1, GROUP, MEMBER_B, 2, &[]).await;
        (s2, r)
    });
    ready_rx.await.expect("m2 about to sync");
    let t = scenario_common::perf::now();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut sync_b).await.is_err(),
        "the follower's SyncGroup must park until the leader synced"
    );
    scenario_common::perf::at_least("kassign/follower-parked", t, Duration::from_millis(250));
    let (err, own) = sync_v1(
        &mut s1,
        next(&mut c1),
        GROUP,
        &m1,
        2,
        &[(m1.as_str(), SLICE_LEADER), (MEMBER_B, SLICE_FOLLOWER), ("ghost", b"ignored")],
    )
    .await;
    assert_eq!((err, own), (ERR_NONE, SLICE_LEADER.to_vec()), "leader sync: its own slice back");
    let (mut s2, late) = tokio::time::timeout(Duration::from_secs(5), sync_b)
        .await
        .expect("the parked follower sync returned within 5s")
        .expect("m2 sync task alive");
    let mut c2 = 2;
    assert_eq!(late, (ERR_NONE, SLICE_FOLLOWER.to_vec()), "the follower receives exactly its own bytes");
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (d.state.as_str(), d.member_ids.clone()),
        ("Stable", vec![m1.clone(), MEMBER_B.to_string()]),
        "stable pair after the leader's sync"
    );
    // 3) Stable re-sync: the stored slice again, no parking.
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 2, &[]).await;
    assert_eq!((err, own), (ERR_NONE, SLICE_FOLLOWER.to_vec()), "stable re-sync returns the stored assignment");
    assert_eq!(heartbeat_v0(&mut s1, next(&mut c1), GROUP, 2, &m1).await, ERR_NONE, "the leader's heartbeat is clean while Stable");
    scenario_common::perf::within("kassign/sync-handoff", t, Duration::from_secs(2));

    // 4) Error ladder: stale generation 22, a stranger 25 even at the right
    //    generation, an unknown group 25, an empty group 24, a foreign
    //    protocol_type 23 -- and the socket/group survive the 23.
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 1, &[]).await;
    assert_eq!((err, own), (ERR_ILLEGAL_GENERATION, Vec::new()), "stale generation");
    let mut sx = connect(&front.kafka, "the stranger").await;
    let mut cx = 0;
    let (err, own) = sync_v1(&mut sx, next(&mut cx), GROUP, "stranger", 2, &[]).await;
    assert_eq!((err, own), (ERR_UNKNOWN_MEMBER_ID, Vec::new()), "not enrolled, even at the current generation");
    let (err, own) = sync_v1(&mut sx, next(&mut cx), GHOST_GROUP, MEMBER_B, 2, &[]).await;
    assert_eq!((err, own), (ERR_UNKNOWN_MEMBER_ID, Vec::new()), "unknown group");
    let (err, own) = sync_v1(&mut sx, next(&mut cx), "", MEMBER_B, 2, &[]).await;
    assert_eq!((err, own), (ERR_INVALID_GROUP_ID, Vec::new()), "empty group id");
    let (err, _, _, _, _) = join_v1_proto(&mut sx, next(&mut cx), GROUP, "connect-1", "connect", "range", b"sub").await;
    assert_eq!(err, ERR_INCONSISTENT_GROUP_PROTOCOL, "a foreign protocol_type cannot enroll");
    assert_eq!(heartbeat_v0(&mut sx, next(&mut cx), GROUP, 2, MEMBER_B).await, ERR_NONE, "the socket that ate the 23 still works; identity is server-side");
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (d.state.as_str(), d.protocol_type.as_str(), d.member_ids.len()),
        ("Stable", "consumer", 2),
        "the rejected join changed nothing"
    );

    // 5) A third member joins under another protocol NAME: the kick makes
    //    EVERY member rejoin (m2 on its own socket in a task, then m1); the
    //    group protocol stays the leader's FIRST protocol ("range") and
    //    generation 3 hands every member the leader's new (here empty) slices.
    assert_eq!(heartbeat_v0(&mut s2, next(&mut c2), GROUP, 2, MEMBER_B).await, ERR_NONE, "m2 alive before the third join");
    let addr = front.kafka.clone();
    let join_c = tokio::spawn(async move {
        let mut sc = connect(&addr, "m3").await;
        let view = join_v1_proto(&mut sc, 1, GROUP, MEMBER_C, "consumer", "roundrobin", SUB_C).await;
        (sc, view)
    });
    poll_describe(&mut probe, &mut cp, |d| d.state == "PreparingRebalance" && d.member_ids.len() == 3, "m3's kick").await;
    assert_eq!(heartbeat_v0(&mut s1, next(&mut c1), GROUP, 2, &m1).await, ERR_REBALANCE_IN_PROGRESS, "m1 is told to rejoin again");
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let mut s2 = s2;
    let rejoin_b = tokio::spawn(async move {
        ready_tx.send(()).expect("test alive");
        let view = join_v1(&mut s2, 20, GROUP, SESSION_MS, REBALANCE_MS, MEMBER_B, SUB_B).await;
        (s2, view)
    });
    ready_rx.await.expect("m2 about to rejoin");
    let third = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!((third.error, third.generation, third.leader.as_str()), (ERR_NONE, 3, m1.as_str()), "third barrier once every member rejoined");
    assert_eq!(
        rows(&third),
        vec![(m1.clone(), SUB_A.to_vec()), (MEMBER_B.to_string(), SUB_B.to_vec()), (MEMBER_C.to_string(), SUB_C.to_vec())],
        "three rows in join order"
    );
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!((d.state.as_str(), d.protocol.as_deref()), ("CompletingSync", Some("range")), "group protocol = the leader's first protocol");
    let (mut s2, view_b3) = tokio::time::timeout(Duration::from_secs(5), rejoin_b)
        .await
        .expect("m2's rejoin returned within 5s")
        .expect("m2 rejoin task alive");
    assert_eq!((view_b3.error, view_b3.generation, view_b3.leader.as_str(), view_b3.members.len()), (ERR_NONE, 3, m1.as_str(), 0), "m2's rejoin reply: generation 3, no roster");
    let (mut s3, view_c) = tokio::time::timeout(Duration::from_secs(5), join_c)
        .await
        .expect("m3's parked join returned within 5s")
        .expect("m3 join task alive");
    assert_eq!((view_c.0, view_c.1, view_c.2.as_deref(), view_c.3.as_str(), view_c.4.len()), (ERR_NONE, 3, Some("range"), m1.as_str(), 0), "m3's reply: gen 3 under the group protocol, no roster");
    let mut c3 = 2;
    let (err, own) = sync_v1(&mut s1, next(&mut c1), GROUP, &m1, 3, &[(m1.as_str(), b"p0"), (MEMBER_B, b""), (MEMBER_C, b"")]).await;
    assert_eq!((err, own), (ERR_NONE, b"p0".to_vec()), "leader sync of the new plan");
    let (err, own) = sync_v1(&mut s3, next(&mut c3), GROUP, MEMBER_C, 3, &[]).await;
    assert_eq!((err, own), (ERR_NONE, Vec::new()), "m3's stored slice is empty");
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 3, &[]).await;
    assert_eq!((err, own), (ERR_NONE, Vec::new()), "m2's old slice was replaced by the new plan");
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!((d.state.as_str(), d.member_ids.len()), ("Stable", 3), "stable trio");

    // 6) Data leg: the assigned fleet reads the whole log in order on the
    //    leader's socket; nothing was committed; the RESP stream agrees.
    let records: Vec<BatchRecord> = VALUES
        .iter()
        .map(|v| BatchRecord { timestamp_delta: 0, key: Some(b"assign"), value: Some(v.as_bytes()), headers: vec![] })
        .collect();
    assert_eq!(kafka_produce_v3(&front.kafka, 7, TOPIC, &records).await, 0, "base offset of the produce");
    let (err, hwm, batch) = fetch_v0(&mut s1, next(&mut c1), TOPIC, 0).await;
    assert_eq!((err, hwm, batch.base_offset, batch.records.len()), (ERR_NONE, 3, 0, 3), "fetch from 0: all three records");
    let got: Vec<&[u8]> = batch.records.iter().map(|rec| rec.value.as_deref().unwrap_or(&[])).collect();
    assert_eq!(got, VALUES.iter().map(|v| v.as_bytes()).collect::<Vec<_>>(), "values in offset order");
    assert_eq!(int_of(&r(&resp, &[b"XLEN", STREAM.as_bytes()]).await), 3, "XLEN on the RESP face = log end");
    assert_eq!(
        fetch_offset_v0_raw(&mut probe, next(&mut cp), GROUP, TOPIC, 0).await,
        (ERR_NONE, -1),
        "no commits in this rehearsal"
    );
    scenario_common::perf::scenario(TAG, t0);
}
