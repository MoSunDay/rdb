//! Multi-store scenario #8 -- IoT telemetry over TWO fronts of the SAME
//! engine: devices upload through the Kafka front (hand-framed Produce
//! v3; the broker decodes RecordBatch v2 into stream entries per
//! src/kafka/produce.rs), and the platform reads the SAME stream over
//! RESP (XRANGE mutual proof). State then fans out per device: a JSON
//! shadow document, a Hash of latest readings, and a ZSET alarm board
//! keyed by temperature score.

use crate::scenario_common;
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{front_world, int_of, ok, r, rb, rf, stream_ids, FrontSpec};

#[tokio::test]
async fn iot_uplink_kafka_downlink_resp_with_shadow_and_alarms() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node with the Kafka front enabled; the RESP port pre-seeds
    //    the topic's queue so Produce v3 has a stream to append to.
    let spec = FrontSpec {
        kafka: true,
        ..Default::default()
    };
    let (front, a) = front_world("iot8", &spec).await;
    let (kafka, resp) = (front.kafka.clone(), a.clone());
    let seeded = r(&resp, &[b"XADD", b"iot/q0", b"1-1", b"seed", b"boot"]).await;
    assert!(
        scenario_common::bulk_at(&seeded, 0).contains("1-1"),
        "seed id echo"
    );

    // b) Three uplinks through Kafka Produce v3 (acks=1): two devices,
    //    JSON payloads, one keyed tombstone-free batch.
    let readings = [
        (r#"{"temp":42,"hum":60}"#, "dev1"),
        (r#"{"temp":21,"hum":55}"#, "dev2"),
        (r#"{"temp":47,"hum":62}"#, "dev1"),
    ];
    let records: Vec<rdb::kafka::record::BatchRecord<'_>> = readings
        .iter()
        .map(|(payload, dev)| rdb::kafka::record::BatchRecord {
            timestamp_delta: 0,
            key: Some(dev.as_bytes()),
            value: Some(payload.as_bytes()),
            headers: vec![],
        })
        .collect();
    let base = kafka_produce_v3(&kafka, 7, "iot", &records).await;
    assert_eq!(base, 1, "base_offset = pre-append len (the seed entry)");

    // c) Mutual proof: the RESP side sees the SAME data the Kafka front
    //    appended -- 4 entries total, the produced ones carrying the k/v
    //    field split of record_fields().
    assert_eq!(int_of(&r(&resp, &[b"XLEN", b"iot/q0"]).await), 4);
    let xrange = rf(&resp, &[b"XRANGE", b"iot/q0", b"-", b"+"]).await;
    for (payload, _dev) in &readings {
        assert!(
            xrange.contains(payload),
            "payload {payload} visible: {xrange}"
        );
    }
    assert!(
        xrange.contains("hum"),
        "decoded fields, not raw batch bytes: {xrange}"
    );
    assert_eq!(stream_ids(&xrange).len(), 4, "four entries by id");

    // d) Per-device fan-out on the RESP side: JSON shadow (latest state
    //    overwrite), Hash of latest scalars, alarm board by temperature.
    ok(
        &r(
            &resp,
            &[
                b"JSON.SET",
                b"shadow:dev1",
                b"$",
                br#"{"temp":47,"hum":62}"#,
            ],
        )
        .await,
        "shadow dev1",
    );
    assert_eq!(
        int_of(
            &r(
                &resp,
                &[b"HSET", b"latest:dev1", b"temp", b"47", b"hum", b"62"]
            )
            .await
        ),
        2
    );
    assert_eq!(
        int_of(
            &r(
                &resp,
                &[b"HSET", b"latest:dev2", b"temp", b"21", b"hum", b"55"]
            )
            .await
        ),
        2
    );
    for (dev, temp) in [("dev1", 47.0f64), ("dev2", 21.0)] {
        assert_eq!(
            int_of(
                &r(
                    &resp,
                    &[
                        b"ZADD",
                        b"alarm:temp",
                        temp.to_string().as_bytes(),
                        dev.as_bytes()
                    ]
                )
                .await
            ),
            1
        );
    }

    // e) The alarm query: only devices at/over the 40-degree threshold.
    let hot = rf(&resp, &[b"ZRANGEBYSCORE", b"alarm:temp", b"40", b"+inf"]).await;
    assert!(hot.contains("dev1") && !hot.contains("dev2"), "{hot}");

    // f) The shadow answers as a JSON document and agrees with the Hash.
    let shadow = rb(&resp, &[b"JSON.GET", b"shadow:dev1", b"$"]).await;
    assert!(shadow.contains("47") && shadow.contains("62"), "{shadow}");
    assert_eq!(rb(&resp, &[b"HGET", b"latest:dev1", b"temp"]).await, "47");
}
