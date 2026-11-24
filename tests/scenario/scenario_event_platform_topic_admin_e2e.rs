//! Batch 11 scenario #113 -- an event platform's topic admin on the
//! Kafka face: CreateTopics materializes `events` with 3 partitions, a
//! duplicate create is 36, CreatePartitions grows it to 5, DescribeConfigs
//! answers the documented 4-entry TOPIC config set, ListOffsets /
//! OffsetForLeaderEpoch read the empty watermarks, and DeleteTopics
//! retires it (Metadata then reports no partitions). The RESP plane sees
//! the same 5 partition queues (`XINFO TOPICS events`), and the topic
//! registry is a SQL table. One REAL node: Kafka + MySQL + RESP fronts.

use std::time::Duration;

use crate::common::mysql::{connect_root, ddl, ints, run};
use crate::kafka_front_common;
use crate::scenario_common;
use crate::scenario_common::{front_world, rf, FrontSpec};
use kafka_front_common::topics::{
    create_partitions, create_topics, delete_topics, describe_configs_topic, list_offsets_v5,
    metadata_partitions, offset_for_leader_epoch,
};
use tokio::net::TcpStream;

const TOPIC: &str = "events";

/// Flat `[child, length]` pairs out of an `XINFO TOPICS` full reply
/// (`*2N` array of a bulk child name followed by an integer length).
fn topic_pairs(full: &str) -> Vec<(String, i64)> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < toks.len() {
        let Some(len) = toks[i].strip_prefix('$') else {
            i += 1;
            continue;
        };
        let child = toks[i + 1];
        let size = toks[i + 2]
            .strip_prefix(':')
            .and_then(|n| n.parse::<i64>().ok());
        if len.parse::<usize>() == Ok(child.len()) {
            if let Some(n) = size {
                out.push((child.to_string(), n));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[tokio::test]
async fn topic_admin_create_grow_describe_delete_and_registry() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        mysql: true,
        ..Default::default()
    };
    let (front, resp) = front_world("topic113", &spec).await;
    let mut sock = TcpStream::connect(&front.kafka)
        .await
        .expect("connect kafka front");

    // a) Ten admin round trips: create, grow, describe, read, retire.
    let t = scenario_common::perf::now();
    assert_eq!(
        create_topics(&mut sock, 1, TOPIC, 3, -1, false).await,
        0,
        "create 3"
    );
    assert_eq!(
        metadata_partitions(&mut sock, 2, TOPIC).await,
        Some(3),
        "metadata sees 3 partitions"
    );
    assert_eq!(
        create_topics(&mut sock, 3, TOPIC, 3, -1, false).await,
        36,
        "duplicate create is TOPIC_ALREADY_EXISTS(36)"
    );
    assert_eq!(
        create_partitions(&mut sock, 4, TOPIC, 5).await,
        0,
        "grow to 5"
    );
    assert_eq!(
        metadata_partitions(&mut sock, 5, TOPIC).await,
        Some(5),
        "metadata sees 5 partitions"
    );
    let (err, cfgs) = describe_configs_topic(&mut sock, 6, TOPIC).await;
    assert_eq!(err, 0, "describe configs clean");
    assert!(cfgs.len() >= 4, "at least four config entries: {cfgs:?}");
    for (name, value) in [
        ("cleanup.policy", "delete"),
        ("retention.ms", "604800000"),
        ("retention.bytes", "-1"),
        ("min.insync.replicas", "1"),
    ] {
        assert!(
            cfgs.iter().any(|(k, v)| k == name && v == value),
            "config {name}={value} missing from {cfgs:?}"
        );
    }
    let (err, _ts, hwm, _epoch) = list_offsets_v5(&mut sock, 7, TOPIC, 0, -1).await;
    assert_eq!((err, hwm), (0, 0), "latest offset of an empty partition");
    let (err, _epoch, end) = offset_for_leader_epoch(&mut sock, 8, TOPIC, 0).await;
    assert_eq!((err, end), (0, 0), "end offset of an empty partition");
    assert_eq!(delete_topics(&mut sock, 9, TOPIC).await, 0, "delete clean");
    assert_eq!(
        metadata_partitions(&mut sock, 10, TOPIC).await,
        None,
        "metadata reports the retired topic as an error row"
    );
    scenario_common::perf::within("topic113/10-admin-rounds", t, Duration::from_secs(3));

    // b) The RESP plane sees the partition queues the admin face made.
    //    Probed on a FRESH `events` handed all five partitions in one
    //    CreateTopics call: the admin block above retired the topic, and
    //    a deleted topic has no queues left to list.
    let mut sock = TcpStream::connect(&front.kafka)
        .await
        .expect("reconnect kafka front");
    assert_eq!(
        create_topics(&mut sock, 11, TOPIC, 5, -1, false).await,
        0,
        "re-create with 5 partitions"
    );
    let info = rf(&resp, &[b"XINFO", b"TOPICS", TOPIC.as_bytes()]).await;
    let pairs = topic_pairs(&info);
    assert_eq!(pairs.len(), 5, "five partition queues: {info}");
    for (idx, (child, len)) in pairs.iter().enumerate() {
        assert_eq!(
            child.as_str(),
            format!("p{idx}"),
            "partition queue {idx} of {info}"
        );
        assert_eq!(*len, 0, "queue {child} is empty");
    }

    // c) The topic registry in SQL: one row, read back verbatim.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE topic_registry(name VARCHAR(32) PRIMARY KEY, partitions INT)",
    )
    .await;
    run(
        &mut c,
        &format!("INSERT INTO topic_registry (name, partitions) VALUES ('{TOPIC}', 5)"),
    )
    .await;
    assert_eq!(
        ints(
            &mut c,
            "SELECT partitions FROM topic_registry WHERE name = 'events'"
        )
        .await,
        vec![5],
        "registered partition count"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM topic_registry").await,
        vec![1],
        "one registered topic"
    );
    scenario_common::perf::scenario("topic113", t0);
}
