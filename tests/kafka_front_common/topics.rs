//! Client-side helpers for the topic-admin wire APIs (P3 backfill
//! e2e): hand-encode CreateTopics/DeleteTopics/CreatePartitions/
//! DescribeConfigs/OffsetForLeaderEpoch/ListOffsets-v5 requests the
//! way a Kafka client would, drive them through the REAL binary over
//! TCP, and decode the replies (classic framing only -- the versions
//! the broker advertises for these keys).

// Each e2e binary mounts only the helpers its scenarios drive.
#![allow(dead_code)]

use rdb::kafka::frame::Reader;
use rdb::kafka::frame::{put_array_len, put_i32, put_i64, put_i8, put_string};
use tokio::net::TcpStream;

use super::{kafka_req, kafka_round};

pub const API_CREATE_TOPICS: i16 = 19;
pub const API_DELETE_TOPICS: i16 = 20;
pub const API_OFFSET_FOR_LEADER_EPOCH: i16 = 23;
pub const API_DESCRIBE_CONFIGS: i16 = 32;
pub const API_CREATE_PARTITIONS: i16 = 37;
pub const API_LIST_OFFSETS: i16 = 2;
pub const API_METADATA: i16 = 3;

/// Strip the response header (corr id) off a round payload.
pub fn body_of(payload: &[u8]) -> Reader<'_> {
    let mut r = Reader::new(payload);
    let _corr = r.i32();
    r
}

/// CreateTopics v4 (the librdkafka pick) for one topic; returns its
/// error code. `parts`/`rf` pass through verbatim (-1 = unset).
pub async fn create_topics(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    parts: i32,
    rf: i16,
    validate_only: bool,
) -> i16 {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_i32(&mut b, parts);
    b.extend_from_slice(&rf.to_be_bytes());
    put_array_len(&mut b, 0); // assignments
    put_array_len(&mut b, 0); // configs
    put_i32(&mut b, 1_000); // timeout
    b.push(validate_only as u8);
    let payload = kafka_round(sock, &kafka_req(API_CREATE_TOPICS, 4, corr, false, &b)).await;
    let mut r = body_of(&payload);
    assert_eq!(r.i32(), Some(0), "throttle first on v2+");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(topic));
    r.i16().unwrap()
}

/// DeleteTopics v3 for one topic; returns its error code.
pub async fn delete_topics(sock: &mut TcpStream, corr: i32, topic: &str) -> i16 {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_i32(&mut b, 1_000);
    let payload = kafka_round(sock, &kafka_req(API_DELETE_TOPICS, 3, corr, false, &b)).await;
    let mut r = body_of(&payload);
    assert_eq!(r.i32(), Some(0), "throttle first on v1+");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(topic));
    r.i16().unwrap()
}

/// CreatePartitions v1 to a new TOTAL count; returns the error code.
pub async fn create_partitions(sock: &mut TcpStream, corr: i32, topic: &str, count: i32) -> i16 {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_i32(&mut b, count);
    put_array_len(&mut b, 0); // assignments
    put_i32(&mut b, 1_000); // timeout
    b.push(0u8); // validate_only
    let payload = kafka_round(sock, &kafka_req(API_CREATE_PARTITIONS, 1, corr, false, &b)).await;
    let mut r = body_of(&payload);
    assert_eq!(r.i32(), Some(0), "throttle always first");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(topic));
    r.i16().unwrap()
}

/// Metadata v1 partition count of `topic` (`None` = error row).
pub async fn metadata_partitions(sock: &mut TcpStream, corr: i32, topic: &str) -> Option<usize> {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    let payload = kafka_round(sock, &kafka_req(API_METADATA, 1, corr, false, &b)).await;
    let mut r = body_of(&payload);
    r.i32(); // throttle
    let brokers = r.array_len().unwrap().unwrap_or(0);
    for _ in 0..brokers {
        r.i32();
        r.string();
        r.i32();
        r.nullable_string(); // rack
    }
    r.i32(); // controller
    assert_eq!(r.array_len(), Some(Some(1)), "one topic row");
    let err = r.i16().unwrap();
    assert_eq!(r.string().as_deref(), Some(topic), "topic echo");
    r.boolean(); // is_internal (v1+)
    if err != 0 {
        return None;
    }
    let n = r.array_len().unwrap().unwrap_or(0);
    Some(n)
}

/// One DescribeConfigs v1 TOPIC resource: (error, [(name, value)]).
pub async fn describe_configs_topic(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
) -> (i16, Vec<(String, String)>) {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_i8(&mut b, 2); // TOPIC
    put_string(&mut b, topic);
    rdb::kafka::frame::put_null_array_len(&mut b); // all keys (null)
    b.push(1u8); // include_synonyms
    let payload = kafka_round(sock, &kafka_req(API_DESCRIBE_CONFIGS, 1, corr, false, &b)).await;
    let mut r = body_of(&payload);
    r.i32(); // throttle
    assert_eq!(r.array_len(), Some(Some(1)));
    let err = r.i16().unwrap();
    r.nullable_string();
    r.i8();
    r.string();
    let mut out = Vec::new();
    for _ in 0..r.array_len().unwrap().unwrap_or(0) {
        let name = r.string().unwrap();
        let value = r.nullable_string().unwrap().unwrap_or_default();
        r.boolean(); // read_only
        r.i8(); // config_source
        r.boolean(); // is_sensitive
        r.array_len(); // synonyms
        out.push((name, value));
    }
    (err, out)
}

/// OffsetForLeaderEpoch v2 for one partition: (error, leader_epoch,
/// end_offset).
pub async fn offset_for_leader_epoch(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    partition: i32,
) -> (i16, i32, i64) {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, partition);
    put_i32(&mut b, -1); // current_leader_epoch
    put_i32(&mut b, 0); // leader_epoch (probe)
    let payload = kafka_round(
        sock,
        &kafka_req(API_OFFSET_FOR_LEADER_EPOCH, 2, corr, false, &b),
    )
    .await;
    let mut r = body_of(&payload);
    assert_eq!(r.i32(), Some(0), "throttle on v2+");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(topic));
    assert_eq!(r.array_len(), Some(Some(1)));
    let err = r.i16().unwrap();
    assert_eq!(r.i32(), Some(partition), "partition echo");
    (err, r.i32().unwrap(), r.i64().unwrap())
}

/// ListOffsets v5 (the version librdkafka negotiates here) for one
/// partition: (error, timestamp, offset, leader_epoch).
pub async fn list_offsets_v5(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    partition: i32,
    timestamp: i64,
) -> (i16, i64, i64, i32) {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    b.push(0u8); // isolation_level
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, partition);
    put_i32(&mut b, -1); // current_leader_epoch
    put_i64(&mut b, timestamp);
    let payload = kafka_round(sock, &kafka_req(API_LIST_OFFSETS, 5, corr, false, &b)).await;
    let mut r = body_of(&payload);
    assert_eq!(r.i32(), Some(0), "throttle first on v2+");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(topic));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(partition), "partition echo");
    (
        r.i16().unwrap(),
        r.i64().unwrap(),
        r.i64().unwrap(),
        r.i32().unwrap(),
    )
}

/// OffsetFetch v0 with a NULL topics array: how many topic sections
/// the group still owns committed offsets for (the ledger view -- a
/// family delete must drop it to zero).
pub async fn fetch_all_sections_v0(sock: &mut TcpStream, corr: i32, group: &str) -> usize {
    let mut b = Vec::new();
    put_string(&mut b, group);
    rdb::kafka::frame::put_null_array_len(&mut b); // topics: null = all
    let payload = kafka_round(sock, &kafka_req(9, 0, corr, false, &b)).await;
    let mut r = body_of(&payload);
    r.array_len().unwrap().unwrap_or(0)
}

/// OffsetFetch v0 round that tolerates an errored partition (a topic
/// deleted after its commits): (partition_error, committed_offset).
pub async fn fetch_offset_v0_raw(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    topic: &str,
    partition: i32,
) -> (i16, i64) {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, partition);
    let payload = kafka_round(sock, &kafka_req(9, 0, corr, false, &b)).await;
    let mut r = body_of(&payload);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(partition));
    let off = r.i64().unwrap();
    r.nullable_string(); // metadata
    (r.i16().unwrap(), off)
}
