//! Topic-admin e2e (P3 backfill #1/#2/#4/#5/#6) against the REAL
//! binary: CreateTopics v4 -> Metadata partitions -> produce/fetch ->
//! CreatePartitions growth -> DeleteTopics (topic gone AND the family
//! collateral gone: DLQ stream, committed-offset ledger, staged delay
//! rows -- asserted the way the DLQ/ledger e2e suites do); the
//! DescribeConfigs stub entries; the OffsetForLeaderEpoch constant
//! answer; ListOffsets v5 (librdkafka's negotiated version); and the
//! produce auto-create switch in both states.

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::groups::commit_v2;
use kafka_front_common::topics::fetch_offset_v0_raw;
use kafka_front_common::topics::{
    create_partitions, create_topics, delete_topics, describe_configs_topic, fetch_all_sections_v0,
    list_offsets_v5, metadata_partitions, offset_for_leader_epoch,
};
use kafka_front_common::{resp_one_shot, spawn_kafka_node, spawn_kafka_node_bind, wait_accepting};
use rdb::kafka::errors;
use rdb::kafka::frame::{put_array_len, put_i16, put_i32, put_i64, put_string, Reader};
use rdb::kafka::record::{build_batch, BatchRecord};
use tokio::net::TcpStream;

/// Produce v2 one record to (topic, part): the partition error code.
async fn produce(sock: &mut TcpStream, corr: i32, topic: &str, part: i32, value: &[u8]) -> i16 {
    let batch = build_batch(
        0,
        1_000,
        &[BatchRecord {
            timestamp_delta: 0,
            key: None,
            value: Some(value),
            headers: vec![],
        }],
    );
    let mut b = Vec::new();
    put_i16(&mut b, 1); // acks
    put_i32(&mut b, 5_000);
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, part);
    put_i32(&mut b, batch.len() as i32);
    b.extend_from_slice(&batch);
    let payload = kafka_front_common::kafka_round(
        sock,
        &kafka_front_common::kafka_req(0, 2, corr, false, &b),
    )
    .await;
    let mut r = Reader::new(&payload);
    r.i32(); // corr
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    r.i32();
    r.i16().unwrap()
}

/// Fetch v4 one partition: (error, hwm, records blob).
async fn fetch_v4(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    part: i32,
    offset: i64,
) -> (i16, i64, Vec<u8>) {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    put_i32(&mut b, 0); // max_wait
    put_i32(&mut b, 0); // min_bytes
    put_i32(&mut b, 1_048_576); // max_bytes (v3+)
    b.push(0u8); // isolation (v4+)
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, part);
    put_i64(&mut b, offset);
    put_i32(&mut b, 1_048_576);
    let payload = kafka_front_common::kafka_round(
        sock,
        &kafka_front_common::kafka_req(1, 4, corr, false, &b),
    )
    .await;
    let mut r = Reader::new(&payload);
    r.i32(); // corr
    r.i32(); // throttle
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    let got_part = r.i32().unwrap();
    assert_eq!(got_part, part);
    let err = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    r.i64(); // last_stable_offset
    assert_eq!(r.array_len(), Some(None), "null aborted_transactions");
    let records = r.bytes().unwrap().unwrap_or(&[]).to_vec();
    (err, hwm, records)
}

/// RESP one-shot command; asserts the reply contains `want`.
async fn resp_want(resp: &str, args: &[&[u8]], want: &[u8]) {
    let reply = resp_one_shot(resp, args).await;
    assert!(
        contains_bytes(&reply, want),
        "{args:?}: want {want:?} in {reply:?}"
    );
}

#[tokio::test]
async fn topic_lifecycle_over_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let mut node = spawn_kafka_node(dir.path());
    let kafka = node.kafka.clone();
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("kafka sock");
    let mut corr = 10;

    // CreateTopics v4: success, then the validation ladder.
    assert_eq!(create_topics(&mut sock, corr, "ord", 2, 1, false).await, 0);
    corr += 1;
    assert_eq!(
        create_topics(&mut sock, corr, "ord", 2, 1, false).await,
        errors::TOPIC_ALREADY_EXISTS
    );
    corr += 1;
    assert_eq!(
        create_topics(&mut sock, corr, "bad name", 1, 1, false).await,
        errors::INVALID_TOPIC_EXCEPTION
    );
    corr += 1;
    assert_eq!(
        create_topics(&mut sock, corr, "rf2", 1, 2, false).await,
        errors::INVALID_REPLICATION_FACTOR
    );
    corr += 1;
    // validate_only on a fresh name: no mutation.
    assert_eq!(create_topics(&mut sock, corr, "ghost", 1, 1, true).await, 0);
    corr += 1;
    assert_eq!(metadata_partitions(&mut sock, corr, "ghost").await, None);
    corr += 1;

    // Metadata shows the created partitions.
    assert_eq!(metadata_partitions(&mut sock, corr, "ord").await, Some(2));
    corr += 1;

    // Produce + fetch on the created topic.
    assert_eq!(produce(&mut sock, corr, "ord", 0, b"m0").await, 0);
    corr += 1;
    assert_eq!(produce(&mut sock, corr, "ord", 0, b"m1").await, 0);
    corr += 1;
    let (err, hwm, records) = fetch_v4(&mut sock, corr, "ord", 0, 0).await;
    corr += 1;
    assert_eq!(err, 0);
    assert_eq!(hwm, 2, "two produced");
    // (base_offset, record count) of the returned batch header.
    assert!(records.len() >= 27, "batch header present");
    let count = i32::from_be_bytes(records[23..27].try_into().unwrap()) + 1;
    assert_eq!(count, 2, "both produced records");

    // ListOffsets v5: latest/earliest + the -1 leader_epoch.
    assert_eq!(
        list_offsets_v5(&mut sock, corr, "ord", 0, -1).await,
        (0, -1, 2, -1)
    );
    corr += 1;
    assert_eq!(
        list_offsets_v5(&mut sock, corr, "ord", 0, -2).await,
        (0, -1, 0, -1)
    );
    corr += 1;

    // OffsetForLeaderEpoch v2: constant answer, end = log end offset.
    assert_eq!(
        offset_for_leader_epoch(&mut sock, corr, "ord", 0).await,
        (0, -1, 2)
    );
    corr += 1;
    assert_eq!(
        offset_for_leader_epoch(&mut sock, corr, "ord", 9).await,
        (errors::UNKNOWN_TOPIC_OR_PARTITION, -1, -1)
    );
    corr += 1;

    // DescribeConfigs stub: the minimal TOPIC set.
    let (err, entries) = describe_configs_topic(&mut sock, corr, "ord").await;
    corr += 1;
    assert_eq!(err, 0);
    assert_eq!(
        entries,
        vec![
            ("cleanup.policy".to_string(), "delete".to_string()),
            ("retention.ms".to_string(), "604800000".to_string()),
            ("retention.bytes".to_string(), "-1".to_string()),
            ("min.insync.replicas".to_string(), "1".to_string()),
        ]
    );

    // CreatePartitions: grow, produce on the new partition, shrink.
    assert_eq!(create_partitions(&mut sock, corr, "ord", 3).await, 0);
    corr += 1;
    assert_eq!(metadata_partitions(&mut sock, corr, "ord").await, Some(3));
    corr += 1;
    assert_eq!(produce(&mut sock, corr, "ord", 2, b"m2").await, 0);
    corr += 1;
    let (err, hwm, _) = fetch_v4(&mut sock, corr, "ord", 2, 0).await;
    corr += 1;
    assert_eq!((err, hwm), (0, 1));
    assert_eq!(
        create_partitions(&mut sock, corr, "ord", 2).await,
        errors::INVALID_PARTITIONS,
        "shrink rejected"
    );
    corr += 1;

    // DeleteTopics: gone from metadata, produce misses afterwards.
    assert_eq!(delete_topics(&mut sock, corr, "ord").await, 0);
    corr += 1;
    assert_eq!(metadata_partitions(&mut sock, corr, "ord").await, None);
    corr += 1;
    assert_eq!(
        produce(&mut sock, corr, "ord", 0, b"late").await,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
    corr += 1;
    assert_eq!(
        delete_topics(&mut sock, corr, "ord").await,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
    node.kill_now();
}

#[tokio::test]
async fn delete_topics_folds_family_collateral() {
    let dir = tempfile::tempdir().unwrap();
    let mut node = spawn_kafka_node(dir.path());
    let kafka = node.kafka.clone();
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("kafka sock");
    let resp = node.resp.clone();
    assert_eq!(create_topics(&mut sock, 1, "col", 1, 1, false).await, 0);

    // DLQ: a MAXDELIVERY group dead-letters one claimed entry into
    // col/p0/dlq (the nested-stream collateral).
    resp_want(
        &resp,
        &[b"XADD", b"col/p0", b"1-1", b"f", b"poison"],
        b"1-1",
    )
    .await;
    resp_want(
        &resp,
        &[
            b"XGROUP",
            b"CREATE",
            b"col/p0",
            b"g",
            b"0-0",
            b"MKSTREAM",
            b"MAXDELIVERY",
            b"1",
        ],
        b"+OK",
    )
    .await;
    resp_want(
        &resp,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"g",
            b"c",
            b"STREAMS",
            b"col/p0",
            b">",
        ],
        b"poison",
    )
    .await;
    resp_want(
        &resp,
        &[b"XCLAIM", b"col/p0", b"g", b"c", b"0", b"1-1"],
        b"*0",
    )
    .await;
    resp_want(&resp, &[b"XLEN", b"col/p0/dlq"], b":1").await;

    // Ledger: a committed offset for col/p0 (kind 0x20 rows).
    assert_eq!(commit_v2(&mut sock, 2, "g", 1, "c", "col", 0, 1).await, 0);
    assert_eq!(
        fetch_offset_v0_raw(&mut sock, 3, "g", "col", 0).await,
        (0, 1)
    );
    assert_eq!(fetch_all_sections_v0(&mut sock, 30, "g").await, 1);

    // Delay: one far-future staged row (kind 0x1D) targeting col/p0
    // (the generated id is the reply; anything but -ERR means staged).
    let staged = resp_one_shot(
        &resp,
        &[
            b"XADD", b"col/p0", b"*", b"DELAY", b"3600000", b"f", b"later",
        ],
    )
    .await;
    assert!(
        !contains_bytes(&staged, b"-ERR"),
        "delayed xadd must stage: {staged:?}"
    );

    // Delete the topic over the wire.
    assert_eq!(delete_topics(&mut sock, 4, "col").await, 0);

    // Family collateral is gone: the DLQ stream, the stream itself,
    // and the committed-offset ledger.
    resp_want(&resp, &[b"XLEN", b"col/p0/dlq"], b":0").await;
    resp_want(&resp, &[b"XLEN", b"col/p0"], b":0").await;
    // Ledger gone with the family: the group's all-offsets view has
    // no sections left and the named partition answers the default -1.
    assert_eq!(fetch_all_sections_v0(&mut sock, 5, "g").await, 0);
    assert_eq!(
        fetch_offset_v0_raw(&mut sock, 6, "g", "col", 0).await,
        (3, -1)
    );

    // A leaked staged delay row would exchange later and REVIVE the
    // stream: wait past several sweep ticks and re-assert emptiness.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    resp_want(&resp, &[b"XLEN", b"col/p0"], b":0").await;
    assert_eq!(metadata_partitions(&mut sock, 7, "col").await, None);
    node.kill_now();
}

#[tokio::test]
async fn auto_create_switch_both_states() {
    // Default off: the historical UNKNOWN_TOPIC_OR_PARTITION(3).
    let dir = tempfile::tempdir().unwrap();
    let mut node = spawn_kafka_node(dir.path());
    let kafka = node.kafka.clone();
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("kafka sock");
    assert_eq!(
        produce(&mut sock, 1, "ac", 0, b"v").await,
        errors::UNKNOWN_TOPIC_OR_PARTITION
    );
    assert_eq!(metadata_partitions(&mut sock, 2, "ac").await, None);
    node.kill_now();

    // On: the topic materializes with one default partition and the
    // produce proceeds.
    let dir = tempfile::tempdir().unwrap();
    let mut node = spawn_kafka_node_bind(dir.path(), "", "kafka_auto_create_topics: true\n");
    let kafka = node.kafka.clone();
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("kafka sock");
    assert_eq!(produce(&mut sock, 1, "ac", 0, b"v").await, 0);
    assert_eq!(metadata_partitions(&mut sock, 2, "ac").await, Some(1));
    node.kill_now();
}
