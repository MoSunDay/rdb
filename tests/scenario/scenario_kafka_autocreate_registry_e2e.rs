//! Batch 11 scenario #126 -- a multi-tenant platform with Kafka
//! auto-create on: a Produce v3 to the unknown topic `auto126`
//! materializes it (base offset 0, then 1 on the next batch), Metadata
//! answers one partition, the RESP plane reads the two records back
//! (`XLEN auto126/p0` == 2, `XINFO TOPICS auto126` == [p0, 2]), a
//! duplicate CreateTopics is 36, DeleteTopics retires it, and the tenant
//! registry is a SQL table. Ten topic round trips close the loop. One
//! REAL node: Kafka + MySQL + RESP fronts.

use std::time::Duration;

use crate::common::mysql::{connect_root, ddl, ints, run};
use crate::kafka_front_common;
use crate::scenario_common;
use crate::scenario_common::kafka::kafka_produce_v3;
use crate::scenario_common::{front_world, int_of, r, rf, FrontSpec};
use kafka_front_common::topics::{
    create_partitions, create_topics, delete_topics, list_offsets_v5, metadata_partitions,
    offset_for_leader_epoch,
};
use tokio::net::TcpStream;

const TOPIC: &str = "auto126";

/// One Produce v3 record with `value`.
fn record(value: &[u8]) -> rdb::kafka::record::BatchRecord<'_> {
    rdb::kafka::record::BatchRecord {
        timestamp_delta: 0,
        key: Some(b"auto"),
        value: Some(value),
        headers: vec![],
    }
}

/// Flat `[child, length]` pairs out of an `XINFO TOPICS` full reply.
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
async fn auto_created_topic_produces_lists_and_registers() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        mysql: true,
        extra_yaml: "kafka_auto_create_topics: true\n".to_string(),
        ..Default::default()
    };
    let (front, resp) = front_world("auto126", &spec).await;

    // a) Ten topic round trips against a topic nobody declared: two
    //    Produce v3 batches materialize it (base 0, then 1), Metadata
    //    answers the default single partition, the watermarks read 2,
    //    CreatePartitions grows it, a duplicate CreateTopics is 36 and
    //    DeleteTopics retires it. The RESP probe below rides in the
    //    middle of the block (it must see the topic before the delete).
    let t = scenario_common::perf::now();
    assert_eq!(
        kafka_produce_v3(&front.kafka, 1, TOPIC, &[record(b"tenant-a")]).await,
        0,
        "auto-created topic starts at offset 0"
    );
    assert_eq!(
        kafka_produce_v3(&front.kafka, 2, TOPIC, &[record(b"tenant-b")]).await,
        1,
        "second batch appends after the first"
    );
    let mut sock = TcpStream::connect(&front.kafka)
        .await
        .expect("connect kafka front");
    assert_eq!(
        metadata_partitions(&mut sock, 3, TOPIC).await,
        Some(1),
        "the auto-created topic has one default partition"
    );
    let (err, _ts, hwm, _epoch) = list_offsets_v5(&mut sock, 4, TOPIC, 0, -1).await;
    assert_eq!((err, hwm), (0, 2), "latest offset = both produced records");
    let (err, _epoch, end) = offset_for_leader_epoch(&mut sock, 5, TOPIC, 0).await;
    assert_eq!((err, end), (0, 2), "end offset = the same watermark");

    // b) The RESP plane reads the two records back on the materialized
    //    partition queue.
    assert_eq!(
        int_of(&r(&resp, &[b"XLEN", b"auto126/p0"]).await),
        2,
        "both produced records are stream entries"
    );
    let info = rf(&resp, &[b"XINFO", b"TOPICS", TOPIC.as_bytes()]).await;
    assert_eq!(
        topic_pairs(&info),
        vec![("p0".to_string(), 2)],
        "one queue holding both records: {info}"
    );

    assert_eq!(
        create_partitions(&mut sock, 6, TOPIC, 2).await,
        0,
        "the platform can grow it afterwards"
    );
    assert_eq!(
        metadata_partitions(&mut sock, 7, TOPIC).await,
        Some(2),
        "metadata sees the grown partition count"
    );
    assert_eq!(
        create_topics(&mut sock, 8, TOPIC, 1, -1, false).await,
        36,
        "duplicate CreateTopics is TOPIC_ALREADY_EXISTS(36)"
    );
    assert_eq!(delete_topics(&mut sock, 9, TOPIC).await, 0, "delete clean");
    assert_eq!(
        metadata_partitions(&mut sock, 10, TOPIC).await,
        None,
        "metadata reports the retired topic as an error row"
    );
    scenario_common::perf::within("auto126/10-topic-rounds", t, Duration::from_secs(5));

    // c) The tenant registry in SQL: the auto-created topic is booked
    //    like any other.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE tenant_topics(name VARCHAR(32) PRIMARY KEY, partitions INT, auto_created INT)",
    )
    .await;
    run(
        &mut c,
        &format!(
            "INSERT INTO tenant_topics (name, partitions, auto_created) VALUES ('{TOPIC}', 1, 1)"
        ),
    )
    .await;
    assert_eq!(
        ints(
            &mut c,
            "SELECT partitions FROM tenant_topics WHERE name = 'auto126'"
        )
        .await,
        vec![1],
        "registered partition count"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT auto_created FROM tenant_topics WHERE name = 'auto126'"
        )
        .await,
        vec![1],
        "booked as auto-created"
    );
    scenario_common::perf::scenario("auto126", t0);
}
