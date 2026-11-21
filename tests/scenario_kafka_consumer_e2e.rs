//! Multi-store scenario #31 -- the Kafka wire plane consumes what the
//! RESP plane produces: an order pipeline is seeded through XADD on the
//! parent topic (auto round-robin into the implicit `orders/q0`), then
//! a hand-framed Kafka consumer Fetch v0 reads the whole batch (hwm,
//! base offset, record count), commits a group offset via
//! OffsetCommit v0, re-reads it via OffsetFetch v0, resumes from the
//! committed offset, and an unknown group gets the -1 "no commit"
//! sentinel. One REAL node, RESP + kafka fronts.

mod common;
mod kafka_front_common;
mod scenario_common;

use std::time::{Duration, Instant};

use kafka_front_common::{kafka_req, kafka_round};
use kafka_front_common::groups::{commit_v0, fetch_offset_v0};
use rdb::kafka::frame::{put_array_len, put_i32, put_i64, put_string, Reader};
use scenario_common::{front_world, r, FrontSpec};
use tokio::net::TcpStream;

const API_FETCH: i16 = 1;

/// Fetch v0 round for (topic, partition 0, offset): (error, hwm, records).
async fn fetch_v0(sock: &mut TcpStream, corr: i32, topic: &str, offset: i64) -> (i16, i64, Vec<u8>) {
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
    assert_eq!(r.string().unwrap(), topic, "topic echoed");
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    assert_eq!(r.i32(), Some(0), "partition 0");
    let error = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    let n = r.i32().unwrap_or(-1) as usize;
    let records = r.take(n).unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0, "v0 reply fully drained");
    (error, hwm, records)
}

/// (base_offset, record_count) of a record batch (v2 batch header).
fn batch_head(records: &[u8]) -> (i64, i32) {
    assert!(records.len() >= 27, "batch header present");
    let base = i64::from_be_bytes(records[..8].try_into().unwrap());
    let last_delta = i32::from_be_bytes(records[23..27].try_into().unwrap());
    (base, last_delta + 1)
}

#[tokio::test]
async fn kafka_consumer_fetches_commits_and_resumes() {
    let spec = FrontSpec {
        kafka: true,
        ..Default::default()
    };
    let (front, a) = front_world("kafka31", &spec).await;

    // a) Seed the order pipeline on the RESP plane: XADD on the bare
    //    parent topic round-robins into the implicit single queue
    //    `orders/q0`, which the kafka front serves as `orders` p0.
    //    Each reply is the `*2` id pair (ms + seq).
    for i in 1..=6 {
        let id = r(
            &a,
            &[
                b"XADD",
                b"orders",
                b"*",
                format!("k{i}").as_bytes(),
                format!("wire-order-{i}").as_bytes(),
            ],
        )
        .await;
        assert!(id.starts_with(b"*2"), "xadd {i} replied an id pair: {id:?}");
    }

    // b) A raw consumer connects (no handshake, no SASL) once the
    //    kafka listener accepts.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut sock = loop {
        match TcpStream::connect(&front.kafka).await {
            Ok(s) => break s,
            Err(_) => {
                assert!(Instant::now() < deadline, "kafka front never accepted");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    // c) Full fetch from offset 0: clean reply, hwm at the end, one
    //    batch holding all six orders.
    let (error, hwm, records) = fetch_v0(&mut sock, 1, "orders", 0).await;
    assert_eq!(error, 0, "no partition error");
    assert_eq!(hwm, 6, "high watermark after six XADDs");
    assert_eq!(batch_head(&records), (0, 6), "one batch, six records");
    let body = String::from_utf8_lossy(&records);
    for i in 1..=6 {
        assert!(
            body.contains(&format!("order-{i}")),
            "record {i} value rides the batch"
        );
    }

    // d) The consumer group ledger: commit ship@4, read it back.
    assert_eq!(commit_v0(&mut sock, 2, "ship", "orders", 4).await, 0);
    assert_eq!(fetch_offset_v0(&mut sock, 3, "ship", "orders", 0).await, 4);

    // e) Resume from the committed offset: the tail two orders only.
    let (error, hwm, tail) = fetch_v0(&mut sock, 4, "orders", 4).await;
    assert_eq!(error, 0, "resume fetch clean");
    assert_eq!(hwm, 6, "watermark unchanged");
    assert_eq!(batch_head(&tail), (4, 2), "tail batch: base 4, two records");

    // f) A group that never committed gets the -1 sentinel.
    assert_eq!(fetch_offset_v0(&mut sock, 5, "ghost", "orders", 0).await, -1);
}
