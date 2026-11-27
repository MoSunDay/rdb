//! Multi-store scenario #167 -- a two-member consumer fleet: the leader keeps
//! heartbeating while the follower goes silent; the coordinator evicts the silent
//! member after its session timeout (the 500ms sweep), the leader is told to
//! rebalance (Heartbeat 27), re-joins into a new generation and the fleet is
//! stable again -- observed through DescribeGroups, ListGroups and Heartbeat
//! codes; the evicted member's late calls answer 25 and the fleet's log (the
//! Lite stream under the topic name) is untouched by the churn.
//! SURFACES: Kafka wire + RESP stream. One REAL node, fronts kafka + resp.
//! Deviation: the brief named the lite stream `fleet/q0`; a topic born on the
//! Kafka face (CreateTopics) lives at `fleet/p0` (src/kafka/mapping.rs:25-33
//! `stream_name`), probed first and pinned.

use crate::common;
use crate::kafka_front_common;
use crate::scenario_common;
use kafka_front_common::groups::{
    DescribeView, JoinView, commit_v2, describe_v0, heartbeat_v0, join_v1, list_groups, sync_v1,
};
use kafka_front_common::topics::{create_topics, fetch_offset_v0_raw};
use kafka_front_common::{kafka_req, kafka_round};
use rdb::kafka::frame::{Reader, put_array_len, put_i32, put_i64, put_string};
use rdb::kafka::record::{BatchRecord, RecordBatch, parse_batch};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{FrontSpec, front_world, int_of, r};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

const TAG: &str = "kexpiry";
const GROUP: &str = "fleetcrew";
const PROBE_GROUP: &str = "fleetprobe";
const TOPIC: &str = "fleet";
const STREAM: &str = "fleet/p0";
const MEMBER_B: &str = "fleet-b";
const SUB_A: &[u8] = b"sub[fleet-a]";
const SUB_B: &[u8] = b"sub[fleet-b]";
const SESSION_MS: i32 = 1_500;
const REBALANCE_MS: i32 = 60_000;
const API_FETCH: i16 = 1;
// Kafka error codes pinned by this scenario (local consts).
const ERR_NONE: i16 = 0;
const ERR_UNKNOWN_MEMBER_ID: i16 = 25;
const ERR_INVALID_SESSION_TIMEOUT: i16 = 26;
const ERR_REBALANCE_IN_PROGRESS: i16 = 27;
const ERR_MEMBER_ID_REQUIRED: i16 = 79;
/// What the fleet reads at the end (checked on both faces).
const VALUES: [&str; 2] = ["fleet-alpha", "fleet-beta"];

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

/// Full-drain reply bytes of one RESP command (short quiet gap).
async fn drain(addr: &str, args: &[&[u8]]) -> Vec<u8> {
    common::lite::cmd_full_reply_wait(addr, common::TOKEN, args, 20, 10_000).await
}

/// Every bulk payload of a RESP reply in wire order (the declared length
/// selects the bytes, so the walk is CRLF-safe).
fn bullets(reply: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = reply;
    while let Some(i) = rest.windows(2).position(|w| w == b"\r\n") {
        let line = &rest[..i];
        rest = &rest[i + 2..];
        if let Some(n) = line
            .strip_prefix(b"$")
            .and_then(|l| std::str::from_utf8(l).ok())
            .and_then(|l| l.parse::<usize>().ok())
        {
            if rest.len() >= n {
                out.push(String::from_utf8_lossy(&rest[..n]).into_owned());
                rest = &rest[n..];
            }
        }
    }
    out
}

#[tokio::test]
async fn kafka_session_expiry_rebalance() {
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

    // 0) The fleet's log: a Kafka-born topic IS the Lite stream `fleet/p0`
    //    (probed: `fleet/q0` is only what a RESP bare-parent XADD picks).
    assert_eq!(
        create_topics(&mut probe, next(&mut cp), TOPIC, 1, -1, false).await,
        ERR_NONE,
        "create {TOPIC}: {ctx}"
    );
    assert_eq!(
        int_of(&r(&resp, &[b"XLEN", STREAM.as_bytes()]).await),
        0,
        "the lite stream under the topic name is live and empty"
    );

    // 1) m1 (leader-to-be): the two-phase handshake -- the empty member id
    //    answers 79 + the generated id; the re-join barriers solo at gen 1.
    let mut s1 = connect(&front.kafka, "m1").await;
    let mut c1 = 0;
    let hello = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, "", SUB_A).await;
    assert_eq!(hello.error, ERR_MEMBER_ID_REQUIRED, "empty member id");
    assert_eq!(hello.generation, 0, "no barrier before the rejoin");
    let m1 = hello.member_id.clone();
    assert!(m1.starts_with("rdb-fleetcrew-"), "generated id: {m1}");
    let solo = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (solo.error, solo.generation, solo.leader.as_str()),
        (ERR_NONE, 1, m1.as_str()),
        "solo barrier: generation 1, m1 leads"
    );
    assert_eq!(rows(&solo), vec![(m1.clone(), SUB_A.to_vec())], "m1 alone on the roster");

    // 2) m2 arrives with a client-chosen id: its JoinGroup PARKS (own socket,
    //    own task) until m1 re-joins; m1 is kicked (heartbeat 27) and the
    //    barrier lands generation 2 -- the roster is the leader's only.
    let t = scenario_common::perf::now();
    let addr = front.kafka.clone();
    let join_b = tokio::spawn(async move {
        let mut sb = connect(&addr, "m2").await;
        let view = join_v1(&mut sb, 1, GROUP, SESSION_MS, REBALANCE_MS, MEMBER_B, SUB_B).await;
        (sb, view)
    });
    let kicked = poll_describe(&mut probe, &mut cp, |d| d.state == "PreparingRebalance" && d.member_ids.len() == 2, "the two-member rebalance").await;
    assert_eq!(
        (kicked.state.as_str(), kicked.protocol.as_deref(), kicked.member_ids.clone()),
        ("PreparingRebalance", None, vec![m1.clone(), MEMBER_B.to_string()]),
        "m2's arrival kicked the group; both members listed, no protocol yet"
    );
    assert_eq!(
        heartbeat_v0(&mut s1, next(&mut c1), GROUP, 1, &m1).await,
        ERR_REBALANCE_IN_PROGRESS,
        "m1 is told to rejoin"
    );
    let again = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (again.error, again.generation, again.leader.as_str()),
        (ERR_NONE, 2, m1.as_str()),
        "second barrier: generation 2, the oldest joiner still leads"
    );
    assert_eq!(
        rows(&again),
        vec![(m1.clone(), SUB_A.to_vec()), (MEMBER_B.to_string(), SUB_B.to_vec())],
        "the leader sees both subscriptions, in join order"
    );
    let (mut s2, view_b) = tokio::time::timeout(Duration::from_secs(5), join_b)
        .await
        .expect("m2's parked join returned within 5s")
        .expect("m2 join task alive");
    assert_eq!(
        (view_b.error, view_b.generation, view_b.leader.as_str(), view_b.members.len()),
        (ERR_NONE, 2, m1.as_str(), 0),
        "the follower's reply: generation 2, an empty roster"
    );
    let (err, own) = sync_v1(&mut s1, next(&mut c1), GROUP, &m1, 2, &[(m1.as_str(), b"p0"), (MEMBER_B, b"")]).await;
    assert_eq!((err, own), (ERR_NONE, b"p0".to_vec()), "leader sync: its own slice");
    let mut c2 = 1;
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 2, &[]).await;
    assert_eq!((err, own), (ERR_NONE, Vec::new()), "follower sync: its empty slice");
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (d.state.as_str(), d.protocol.as_deref(), d.member_ids.clone()),
        ("Stable", Some("range"), vec![m1.clone(), MEMBER_B.to_string()]),
        "stable pair"
    );
    assert_eq!(
        list_groups(&mut probe, next(&mut cp), 1, &[]).await,
        vec![(GROUP.to_string(), "consumer".to_string(), "Stable".to_string())],
        "ListGroups v1 shows the fleet Stable"
    );
    scenario_common::perf::within("kexpiry/rejoin-dance", t, Duration::from_secs(2));

    // 3) m2 goes silent: m1 keeps heartbeating (every 100ms) while the test
    //    polls DescribeGroups (every 100ms); the sweep drops m2 once its
    //    session passed and kicks the remainder. Heartbeat codes are recorded.
    let t_evict = scenario_common::perf::now();
    let deadline = Instant::now() + Duration::from_millis(SESSION_MS as u64 + 2_500);
    let mut codes: Vec<i16> = Vec::new();
    let evicted = loop {
        codes.push(heartbeat_v0(&mut s1, next(&mut c1), GROUP, 2, &m1).await);
        let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
        if d.state != "Stable" {
            break d;
        }
        assert!(
            Instant::now() < deadline,
            "m2 was never evicted (session {SESSION_MS}ms); heartbeat codes {codes:?}: {ctx}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    scenario_common::perf::at_least("kexpiry/eviction-honors-session", t_evict, Duration::from_millis(1_000));
    assert_eq!(
        (evicted.state.as_str(), evicted.protocol.as_deref(), evicted.member_ids.clone()),
        ("PreparingRebalance", None, vec![m1.clone()]),
        "the silent member is gone, the survivor is kicked"
    );
    assert_eq!(codes.first(), Some(&ERR_NONE), "clean while m2's session lived: {codes:?}");
    assert_eq!(codes.last(), Some(&ERR_REBALANCE_IN_PROGRESS), "27 after the eviction: {codes:?}");
    let first_kick = codes.iter().position(|c| *c == ERR_REBALANCE_IN_PROGRESS).expect("a 27 was seen");
    assert!(
        codes[..first_kick].iter().all(|c| *c == ERR_NONE)
            && codes[first_kick..].iter().all(|c| *c == ERR_REBALANCE_IN_PROGRESS),
        "0..0 then 27..27, nothing else: {codes:?}"
    );
    assert_eq!(
        list_groups(&mut probe, next(&mut cp), 1, &[]).await,
        vec![(GROUP.to_string(), "consumer".to_string(), "PreparingRebalance".to_string())],
        "ListGroups v1 reports the live stage"
    );

    // 4) The evicted member is a stranger now: 25 on every late call.
    assert_eq!(
        heartbeat_v0(&mut s2, next(&mut c2), GROUP, 2, MEMBER_B).await,
        ERR_UNKNOWN_MEMBER_ID,
        "late heartbeat"
    );
    assert_eq!(
        commit_v2(&mut s2, next(&mut c2), GROUP, 2, MEMBER_B, TOPIC, 0, 1).await,
        ERR_UNKNOWN_MEMBER_ID,
        "late commit fenced"
    );
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 2, &[]).await;
    assert_eq!((err, own), (ERR_UNKNOWN_MEMBER_ID, Vec::new()), "late sync fenced");

    // 5) m1 re-joins alone: generation 3, the fleet is stable again.
    let solo3 = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (solo3.error, solo3.generation, solo3.leader.as_str()),
        (ERR_NONE, 3, m1.as_str()),
        "third barrier: generation 3"
    );
    assert_eq!(rows(&solo3), vec![(m1.clone(), SUB_A.to_vec())], "m1 alone again");
    let (err, own) = sync_v1(&mut s1, next(&mut c1), GROUP, &m1, 3, &[(m1.as_str(), b"p0")]).await;
    assert_eq!((err, own), (ERR_NONE, b"p0".to_vec()), "solo sync");
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (d.state.as_str(), d.protocol.as_deref(), d.member_ids.clone()),
        ("Stable", Some("range"), vec![m1.clone()]),
        "stable solo fleet"
    );
    assert_eq!(heartbeat_v0(&mut s1, next(&mut c1), GROUP, 3, &m1).await, ERR_NONE, "clean heartbeat again");

    // 6) Session timeout validation on a fresh socket: 26 for either bound,
    //    and a rejected join enrolls nothing.
    let mut s3 = connect(&front.kafka, "the timeout probe").await;
    let mut c3 = 0;
    let bad_session = join_v1(&mut s3, next(&mut c3), PROBE_GROUP, 0, REBALANCE_MS, "probe-a", b"sub").await;
    assert_eq!(bad_session.error, ERR_INVALID_SESSION_TIMEOUT, "session_timeout_ms 0");
    let bad_rebalance = join_v1(&mut s3, next(&mut c3), PROBE_GROUP, SESSION_MS, 0, "probe-a", b"sub").await;
    assert_eq!(bad_rebalance.error, ERR_INVALID_SESSION_TIMEOUT, "rebalance_timeout_ms 0");
    let ghost = describe_v0(&mut probe, next(&mut cp), PROBE_GROUP).await;
    assert_eq!((ghost.state.as_str(), ghost.member_ids.len()), ("Dead", 0), "no group from a rejected join");

    // 7) Data plane: two records, fetched in order over m1's socket, the same
    //    entries on the RESP stream, and the evicted member's commit wrote nothing.
    let records: Vec<BatchRecord> = VALUES
        .iter()
        .map(|v| BatchRecord { timestamp_delta: 0, key: None, value: Some(v.as_bytes()), headers: vec![] })
        .collect();
    assert_eq!(kafka_produce_v3(&front.kafka, 7, TOPIC, &records).await, 0, "base offset of the produce");
    let (err, hwm, batch) = fetch_v0(&mut s1, next(&mut c1), TOPIC, 0).await;
    assert_eq!((err, hwm, batch.base_offset, batch.records.len()), (ERR_NONE, 2, 0, 2), "fetch from 0: both records");
    let got: Vec<&[u8]> = batch.records.iter().map(|rec| rec.value.as_deref().unwrap_or(&[])).collect();
    assert_eq!(got, VALUES.iter().map(|v| v.as_bytes()).collect::<Vec<_>>(), "values in offset order");
    let xrange = drain(&resp, &[b"XRANGE", STREAM.as_bytes(), b"-", b"+"]).await;
    let flat = bullets(&xrange);
    assert_eq!(flat.len(), 6, "two entries x (id, v, value): {}", String::from_utf8_lossy(&xrange));
    let ids = scenario_common::stream_ids(&String::from_utf8_lossy(&xrange));
    assert_eq!(ids.len(), 2, "two stream ids: {ids:?}");
    for (i, v) in VALUES.iter().enumerate() {
        assert_eq!((flat[3 * i + 1].as_str(), flat[3 * i + 2].as_str()), ("v", *v), "entry {i} on the RESP face");
    }
    assert_eq!(int_of(&r(&resp, &[b"XLEN", STREAM.as_bytes()]).await), 2, "XLEN = log end");
    assert_eq!(
        fetch_offset_v0_raw(&mut probe, next(&mut cp), GROUP, TOPIC, 0).await,
        (ERR_NONE, -1),
        "the evicted member's commit landed nothing"
    );
    scenario_common::perf::scenario(TAG, t0);
}
