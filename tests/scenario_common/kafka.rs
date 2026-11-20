//! Hand-framed Kafka Produce v3 against the node's `kafka_bind` front
//! (subset of kafka_front_common the IoT scenario needs; kept here so a
//! scenario binary only declares `mod common; mod scenario_common;`).

#![allow(dead_code)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ---- Kafka produce v3 (hand-framed, acks=1) ------------------------

/// One Produce v3 request carrying a hand-built RecordBatch (v2 magic)
/// to `topic`/partition 0; returns the echoed base_offset. Wire shapes
/// per kafka_produce_e2e.rs + src/kafka/produce.rs (v3 adds the null
/// transactional_id; the response appends throttle_time_ms).
pub async fn kafka_produce_v3(
    addr: &str,
    corr: i32,
    topic: &str,
    records: &[rdb::kafka::record::BatchRecord<'_>],
) -> i32 {
    let batch = rdb::kafka::record::build_batch(0, 1_000, records);
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&(-1i16).to_be_bytes()); // transactional_id: null
    body.extend_from_slice(&1i16.to_be_bytes()); // acks
    body.extend_from_slice(&5_000i32.to_be_bytes()); // timeout_ms
    body.extend_from_slice(&1i32.to_be_bytes()); // topics[.]
    body.extend_from_slice(&(topic.len() as i16).to_be_bytes());
    body.extend_from_slice(topic.as_bytes());
    body.extend_from_slice(&1i32.to_be_bytes()); // partitions[.]
    body.extend_from_slice(&0i32.to_be_bytes()); // partition 0
    body.extend_from_slice(&(batch.len() as i32).to_be_bytes());
    body.extend_from_slice(&batch);
    let mut req: Vec<u8> = Vec::new();
    req.extend_from_slice(&0i16.to_be_bytes()); // api: Produce
    req.extend_from_slice(&3i16.to_be_bytes()); // version 3
    req.extend_from_slice(&corr.to_be_bytes());
    req.extend_from_slice(&3i16.to_be_bytes());
    req.extend_from_slice(b"e2e"); // client_id
    req.extend_from_slice(&body);
    let mut sock = TcpStream::connect(addr).await.expect("connect kafka front");
    sock.write_all(&(req.len() as u32).to_be_bytes()).await.expect("frame len");
    sock.write_all(&req).await.expect("write produce v3");
    let mut lb = [0u8; 4];
    sock.read_exact(&mut lb).await.expect("read resp len");
    let mut payload = vec![0u8; i32::from_be_bytes(lb) as usize];
    sock.read_exact(&mut payload).await.expect("read resp body");
    let mut cur = &payload[..];
    let take = |c: &mut &[u8], n: usize| -> Vec<u8> {
        let (head, rest) = c.split_at(n);
        *c = rest;
        head.to_vec()
    };
    let got_corr = i32::from_be_bytes(take(&mut cur, 4).try_into().unwrap());
    assert_eq!(got_corr, corr, "correlation echo");
    let _topics_len = i32::from_be_bytes(take(&mut cur, 4).try_into().unwrap());
    let tlen = i16::from_be_bytes(take(&mut cur, 2).try_into().unwrap()) as usize;
    let _topic = String::from_utf8_lossy(&take(&mut cur, tlen)).into_owned();
    let _parts_len = i32::from_be_bytes(take(&mut cur, 4).try_into().unwrap());
    let _partition = i32::from_be_bytes(take(&mut cur, 4).try_into().unwrap());
    let error = i16::from_be_bytes(take(&mut cur, 2).try_into().unwrap());
    assert_eq!(error, 0, "produce v3 error code");
    i64::from_be_bytes(take(&mut cur, 8).try_into().unwrap()) as i32
}
