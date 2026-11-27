//! Multi-store scenario #169 -- the checkpoint desk: a two-member consumer
//! group's leader commits the group's checkpoint (next-to-consume ordinal
//! 3); commits quoting a stale/future generation (22) and every call of a
//! member that left (25) are fenced and leave it untouched; across the
//! LeaveGroup-triggered rebalance into generation 3 it survives, the solo
//! leader advances it to the log end; the same log reads back on both faces.
//! SURFACES: Kafka wire + RESP stream. One REAL node, fronts kafka + resp.
//! Deviation: the brief named the lite stream `ckpt/q0`; a topic born on
//! the Kafka face (CreateTopics) lives at `ckpt/p0` (src/kafka/mapping.rs:25-33
//! `stream_name`), probed in #167/#168 and pinned here.
//! Deviation: no Fetch session_id/epoch leg -- a known defect suspect
//! (guide addendum 12, src/kafka/fetch.rs:196-199): left out.

use crate::common;
use crate::kafka_front_common;
use crate::scenario_common;
use kafka_front_common::groups::{
    DescribeView, JoinView, commit_v2, describe_v0, heartbeat_v0, join_v1, leave_v0, list_groups,
    sync_v1,
};
use kafka_front_common::topics::{create_topics, fetch_all_sections_v0, fetch_offset_v0_raw};
use kafka_front_common::{kafka_req, kafka_round};
use rdb::kafka::frame::{Reader, put_array_len, put_i32, put_i64, put_string};
use rdb::kafka::record::{BatchRecord, Record, parse_batch};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{FrontSpec, front_world, int_of, r};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

const TAG: &str = "kfence";
const GROUP: &str = "desk";
const TOPIC: &str = "ckpt";
const STREAM: &str = "ckpt/p0";
const MEMBER_B: &str = "desk-b";
const SUB_A: &[u8] = b"sub[desk-m1]";
const SUB_B: &[u8] = b"sub[desk-b]";
const SESSION_MS: i32 = 1_500;
const REBALANCE_MS: i32 = 60_000;
const API_FETCH: i16 = 1;
// Kafka error codes pinned by this scenario (local consts).
const ERR_NONE: i16 = 0;
const ERR_ILLEGAL_GENERATION: i16 = 22;
const ERR_UNKNOWN_MEMBER_ID: i16 = 25;
const ERR_REBALANCE_IN_PROGRESS: i16 = 27;
const ERR_MEMBER_ID_REQUIRED: i16 = 79;
/// The desk's log (read on both faces); the leader checkpoints after the
/// first three records, CHECKPOINT being the next-to-consume ordinal.
const VALUES: [&str; 5] = ["v0", "v1", "v2", "v3", "v4"];
const CHECKPOINT: i64 = 3;

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

/// Fetch v0 round for (topic, partition 0, offset): (error, hwm, records);
/// a 0-length record set (the EOF poll) decodes to no records.
async fn fetch_v0(sock: &mut TcpStream, corr: i32, topic: &str, offset: i64) -> (i16, i64, Vec<Record>) {
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
    let raw = r.take(n).unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0, "v0 reply fully drained");
    let records = if raw.is_empty() {
        Vec::new()
    } else {
        parse_batch(&raw).expect("CRC-verified record batch").records
    };
    (error, hwm, records)
}

/// Record values in offset order.
fn values_of(records: &[Record]) -> Vec<&[u8]> {
    records.iter().map(|rec| rec.value.as_deref().unwrap_or(&[])).collect()
}

/// The leader's member rows of a JoinGroup reply as (id, subscription bytes).
fn rows(view: &JoinView) -> Vec<(String, Vec<u8>)> {
    view.members
        .iter()
        .map(|m| (m.member_id.clone(), m.metadata.clone()))
        .collect()
}

/// Heartbeat v0 round for the desk, pinned to `want`.
async fn beat(sock: &mut TcpStream, corr: &mut i32, generation: i32, member: &str, want: i16, what: &str) {
    assert_eq!(heartbeat_v0(sock, next(corr), GROUP, generation, member).await, want, "{what}");
}

/// OffsetCommit v2 of (topic, partition 0) for the desk: the partition's error code.
async fn commit(sock: &mut TcpStream, corr: &mut i32, generation: i32, member: &str, offset: i64) -> i16 {
    commit_v2(sock, next(corr), GROUP, generation, member, TOPIC, 0, offset).await
}

/// OffsetFetch v0 of the desk's partition: (partition error, committed offset).
async fn offset_of(sock: &mut TcpStream, corr: &mut i32) -> (i16, i64) {
    fetch_offset_v0_raw(sock, next(corr), GROUP, TOPIC, 0).await
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

/// XLEN of the desk's stream on the RESP face.
async fn xlen(resp: &str) -> i64 {
    int_of(&r(resp, &[b"XLEN", STREAM.as_bytes()]).await)
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
async fn kafka_fenced_commit() {
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
    // Derived in Rust from the inputs: the log end and what a checkpoint
    // at CHECKPOINT still owes the desk.
    let log_end = VALUES.len() as i64;
    let owed: Vec<&str> = VALUES[CHECKPOINT as usize..].to_vec();
    let owed_bytes: Vec<&[u8]> = owed.iter().map(|v| v.as_bytes()).collect();
    let all_bytes: Vec<&[u8]> = VALUES.iter().map(|v| v.as_bytes()).collect();

    // 0) The desk's log: five records on the Kafka face are the five
    //    entries of the Lite stream `ckpt/p0` on the RESP face; nobody
    //    has committed anything yet.
    assert_eq!(
        create_topics(&mut probe, next(&mut cp), TOPIC, 1, -1, false).await,
        ERR_NONE,
        "create {TOPIC}: {ctx}"
    );
    assert_eq!(xlen(&resp).await, 0, "the lite stream under the topic name is live and empty");
    let records: Vec<BatchRecord> = VALUES
        .iter()
        .map(|v| BatchRecord { timestamp_delta: 0, key: None, value: Some(v.as_bytes()), headers: vec![] })
        .collect();
    assert_eq!(kafka_produce_v3(&front.kafka, 11, TOPIC, &records).await, 0, "base offset of the produce");
    assert_eq!(xlen(&resp).await, log_end, "the RESP face counts the same log");
    assert_eq!(offset_of(&mut probe, &mut cp).await, (ERR_NONE, -1), "no checkpoint before the desk opened");

    // 1) Formation: m1 passes the two-phase handshake (generation 1); m2
    //    arrives with a client-chosen id and parks; the kick tells m1 to
    //    rejoin (Heartbeat 27) and the barrier hands generation 2 to both.
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
    poll_describe(&mut probe, &mut cp, |d| d.state == "PreparingRebalance" && d.member_ids.len() == 2, "m2's kick")
        .await;
    beat(&mut s1, &mut c1, 1, &m1, ERR_REBALANCE_IN_PROGRESS, "m1 is told to rejoin").await;
    let pair = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (pair.error, pair.generation, pair.leader.as_str()),
        (ERR_NONE, 2, m1.as_str()),
        "second barrier: generation 2, the oldest joiner leads"
    );
    assert_eq!(
        rows(&pair),
        vec![(m1.clone(), SUB_A.to_vec()), (MEMBER_B.to_string(), SUB_B.to_vec())],
        "the leader sees both subscriptions, in join order"
    );
    let (s2, view_b) = tokio::time::timeout(Duration::from_secs(5), join_b)
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
    let mut s2 = s2;
    let mut c2 = 2;
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 2, &[]).await;
    assert_eq!((err, own), (ERR_NONE, Vec::new()), "follower sync: its empty slice");
    let d = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (d.state.as_str(), d.protocol.as_deref(), d.member_ids.len()),
        ("Stable", Some("range"), 2),
        "stable pair"
    );

    // 2) Checkpoint: the leader commits ordinal 3 at generation 2 (0),
    //    OffsetFetch reads it back, and a Fetch FROM the checkpoint
    //    returns exactly the records the desk still owes; the RESP face
    //    shows the same five entries, the checkpointed one included.
    let t = scenario_common::perf::now();
    beat(&mut s1, &mut c1, 2, &m1, ERR_NONE, "the stable pair heartbeats cleanly").await;
    assert_eq!(commit(&mut s1, &mut c1, 2, &m1, CHECKPOINT).await, ERR_NONE, "leader commit at generation 2");
    assert_eq!(offset_of(&mut probe, &mut cp).await, (ERR_NONE, CHECKPOINT), "OffsetFetch reads the checkpoint");
    let (err, hwm, batch) = fetch_v0(&mut s1, next(&mut c1), TOPIC, CHECKPOINT).await;
    assert_eq!((err, hwm, batch.len()), (ERR_NONE, log_end, owed.len()), "fetch from the checkpoint");
    assert_eq!(values_of(&batch), owed_bytes, "exactly the records still owed");
    let xrange = drain(&resp, &[b"XRANGE", STREAM.as_bytes(), b"-", b"+"]).await;
    let flat = bullets(&xrange);
    assert_eq!(flat.len(), 3 * VALUES.len(), "one entry = (id, v, value): {}", String::from_utf8_lossy(&xrange));
    for (i, v) in VALUES.iter().enumerate() {
        assert_eq!((flat[3 * i + 1].as_str(), flat[3 * i + 2].as_str()), ("v", *v), "entry {i} on the RESP face");
    }

    // 3) Generation fence: a stale (1) and a future (3) generation both
    //    answer 22 and write nothing.
    assert_eq!(commit(&mut s1, &mut c1, 1, &m1, 0).await, ERR_ILLEGAL_GENERATION, "stale generation commit fenced");
    assert_eq!(commit(&mut s1, &mut c1, 3, &m1, log_end).await, ERR_ILLEGAL_GENERATION, "future generation fenced");
    assert_eq!(offset_of(&mut probe, &mut cp).await, (ERR_NONE, CHECKPOINT), "the fenced commits wrote nothing");

    // 4) m2 leaves the desk: 0, the survivor is kicked (27), rejoins alone
    //    into generation 3 and the group is Stable with the checkpoint intact.
    assert_eq!(leave_v0(&mut s2, next(&mut c2), GROUP, MEMBER_B).await, ERR_NONE, "m2 leaves");
    let left = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (left.state.as_str(), left.member_ids.clone()),
        ("PreparingRebalance", vec![m1.clone()]),
        "the leaver is gone, the survivor is kicked"
    );
    beat(&mut s1, &mut c1, 2, &m1, ERR_REBALANCE_IN_PROGRESS, "m1 is told to rejoin after the leave").await;
    let solo3 = join_v1(&mut s1, next(&mut c1), GROUP, SESSION_MS, REBALANCE_MS, &m1, SUB_A).await;
    assert_eq!(
        (solo3.error, solo3.generation, solo3.leader.as_str()),
        (ERR_NONE, 3, m1.as_str()),
        "third barrier: generation 3, m1 alone"
    );
    assert_eq!(rows(&solo3), vec![(m1.clone(), SUB_A.to_vec())], "m1 alone again");
    let (err, own) = sync_v1(&mut s1, next(&mut c1), GROUP, &m1, 3, &[(m1.as_str(), b"p0")]).await;
    assert_eq!((err, own), (ERR_NONE, b"p0".to_vec()), "solo sync");
    assert_eq!(offset_of(&mut probe, &mut cp).await, (ERR_NONE, CHECKPOINT), "the checkpoint survives the rebalance");

    // 5) Member fence: the leaver is a stranger whatever generation it
    //    quotes (25 before any generation check) -- commit, heartbeat,
    //    sync and a second leave all bounce off the Stable desk.
    assert_eq!(commit(&mut s2, &mut c2, 3, MEMBER_B, 0).await, ERR_UNKNOWN_MEMBER_ID, "leaver's commit: current gen");
    assert_eq!(commit(&mut s2, &mut c2, 2, MEMBER_B, 0).await, ERR_UNKNOWN_MEMBER_ID, "stale gen: member check first");
    beat(&mut s2, &mut c2, 3, MEMBER_B, ERR_UNKNOWN_MEMBER_ID, "the leaver's heartbeat fenced").await;
    let (err, own) = sync_v1(&mut s2, next(&mut c2), GROUP, MEMBER_B, 3, &[]).await;
    assert_eq!((err, own), (ERR_UNKNOWN_MEMBER_ID, Vec::new()), "the leaver's sync fenced");
    assert_eq!(leave_v0(&mut s2, next(&mut c2), GROUP, MEMBER_B).await, ERR_UNKNOWN_MEMBER_ID, "leaving twice");
    assert_eq!(offset_of(&mut probe, &mut cp).await, (ERR_NONE, CHECKPOINT), "leaver's fenced commits wrote nothing");
    let held = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (held.state.as_str(), held.member_ids.clone()),
        ("Stable", vec![m1.clone()]),
        "the fenced calls changed nothing"
    );

    // 6) The surviving leader catches up: commit at the log end (5) at
    //    generation 3; OffsetFetch reads 5 (one topic section in the
    //    all-view), a Fetch from the new checkpoint (= log end) is the
    //    EOF poll, the whole log is still on both faces and the desk is Stable.
    beat(&mut s1, &mut c1, 3, &m1, ERR_NONE, "the solo desk heartbeats cleanly").await;
    assert_eq!(commit(&mut s1, &mut c1, 3, &m1, log_end).await, ERR_NONE, "leader commit at generation 3");
    assert_eq!(offset_of(&mut probe, &mut cp).await, (ERR_NONE, log_end), "OffsetFetch reads the new checkpoint");
    assert_eq!(fetch_all_sections_v0(&mut probe, next(&mut cp), GROUP).await, 1, "the ledger knows one topic");
    let (err, hwm, batch) = fetch_v0(&mut s1, next(&mut c1), TOPIC, log_end).await;
    assert_eq!((err, hwm, batch.len()), (ERR_NONE, log_end, 0), "nothing left to consume");
    let (err, hwm, batch) = fetch_v0(&mut s1, next(&mut c1), TOPIC, 0).await;
    assert_eq!((err, hwm, batch.len()), (ERR_NONE, log_end, VALUES.len()), "fetch from 0: the whole log");
    assert_eq!(values_of(&batch), all_bytes, "values in offset order");
    assert_eq!(xlen(&resp).await, log_end, "the RESP face still counts the same log");
    let done = describe_v0(&mut probe, next(&mut cp), GROUP).await;
    assert_eq!(
        (done.state.as_str(), done.protocol_type.as_str(), done.protocol.as_deref(), done.member_ids.clone()),
        ("Stable", "consumer", Some("range"), vec![m1.clone()]),
        "stable solo desk"
    );
    assert_eq!(
        list_groups(&mut probe, next(&mut cp), 1, &[]).await,
        vec![(GROUP.to_string(), "consumer".to_string(), "Stable".to_string())],
        "ListGroups v1 shows the desk Stable"
    );
    scenario_common::perf::within("kfence/fence-ladder", t, Duration::from_secs(2));
    scenario_common::perf::scenario(TAG, t0);
}
