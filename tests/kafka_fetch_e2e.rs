//! P2 of the Kafka wire front against the REAL binary: Fetch v0-v10
//! (records, hwm, the version ladder, budget truncation, long-poll
//! wake and deadline expiry). The offset ledger rides
//! `kafka_offsets_e2e.rs`.

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::{kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::frame::{put_array_len, put_i32, put_i64, put_string, Reader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const API_FETCH: i16 = 1;

/// Fetch request body (classic encoding): one topic, one partition.
#[allow(clippy::too_many_arguments)]
fn fetch_body(
    version: i16,
    max_wait: i32,
    min_bytes: i32,
    max_bytes: i32,
    topic: &str,
    partition: i32,
    offset: i64,
    part_max: i32,
) -> Vec<u8> {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    put_i32(&mut b, max_wait);
    put_i32(&mut b, min_bytes);
    if version >= 3 {
        put_i32(&mut b, max_bytes);
    }
    if version >= 4 {
        b.push(0); // isolation_level
    }
    if version >= 7 {
        put_i32(&mut b, 0); // session_id
        put_i32(&mut b, 0); // session_epoch
    }
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, partition);
    if version >= 9 {
        put_i32(&mut b, -1); // current_leader_epoch
    }
    put_i64(&mut b, offset);
    if version >= 5 {
        put_i64(&mut b, -1); // log_start_offset (ignored)
    }
    put_i32(&mut b, part_max);
    if version >= 7 {
        put_array_len(&mut b, 0); // forgotten_topics_data
    }
    b
}

/// Fetch with TWO partitions of one topic (budget test).
fn fetch_two_parts(version: i16, topic: &str, parts: &[(i32, i64, i32)]) -> Vec<u8> {
    let mut b = Vec::new();
    put_i32(&mut b, -1);
    put_i32(&mut b, 0); // max_wait
    put_i32(&mut b, 1); // min_bytes
    if version >= 3 {
        put_i32(&mut b, 200); // global max_bytes: soft budget
    }
    if version >= 4 {
        b.push(0);
    }
    if version >= 7 {
        put_i32(&mut b, 0);
        put_i32(&mut b, 0);
    }
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, parts.len());
    for (p, off, pmax) in parts {
        put_i32(&mut b, *p);
        if version >= 9 {
            put_i32(&mut b, -1); // current_leader_epoch
        }
        put_i64(&mut b, *off);
        if version >= 5 {
            put_i64(&mut b, -1); // log_start_offset
        }
        put_i32(&mut b, *pmax);
    }
    if version >= 7 {
        put_array_len(&mut b, 0);
    }
    b
}

/// One decoded response partition of the first topic.
struct FetchOut {
    throttle: i32,
    session_id: i32,
    top_error: i16,
    partition: i32,
    error: i16,
    hwm: i64,
    lso: i64,
    preferred_replica: i32,
    aborted_null: bool,
    records: Vec<u8>,
}

fn fetch_out(body: &[u8], version: i16) -> FetchOut {
    let mut r = Reader::new(body);
    let mut o = FetchOut {
        throttle: 0,
        session_id: -1,
        top_error: 0,
        partition: 0,
        error: 0,
        hwm: 0,
        lso: 0,
        preferred_replica: -1,
        aborted_null: false,
        records: Vec::new(),
    };
    if version >= 1 {
        o.throttle = r.i32().unwrap_or(-1);
    }
    if version >= 7 {
        o.top_error = r.i16().unwrap_or(-1);
        o.session_id = r.i32().unwrap_or(-1);
    }
    assert_eq!(r.array_len(), Some(Some(1)), "one topic");
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    o.partition = r.i32().unwrap_or(-1);
    o.error = r.i16().unwrap_or(-1);
    o.hwm = r.i64().unwrap_or(-1);
    if version >= 4 {
        o.lso = r.i64().unwrap_or(-1);
    }
    if version >= 5 {
        let _log_start = r.i64().unwrap_or(-1);
    }
    if version >= 4 {
        o.aborted_null = matches!(r.array_len(), Some(None));
    }
    if version >= 11 {
        o.preferred_replica = r.i32().unwrap_or(-2);
    }
    let n = r.i32().unwrap_or(-1) as usize;
    assert_eq!(r.remaining(), n, "records bytes length prefix");
    o.records = r.take(n).unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0, "response fully drained");
    o
}

/// (base_offset, record_count) of a returned record batch.
fn batch_head(records: &[u8]) -> (i64, i32) {
    assert!(records.len() >= 27, "batch header present");
    let base = i64::from_be_bytes(records[..8].try_into().unwrap());
    let last_delta = i32::from_be_bytes(records[23..27].try_into().unwrap());
    (base, last_delta + 1)
}

async fn fetch_round(sock: &mut TcpStream, corr: i32, version: i16, body: &[u8]) -> FetchOut {
    let payload = kafka_round(sock, &kafka_req(API_FETCH, version, corr, false, body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation id echo");
    let hdr = r.pos();
    fetch_out(&payload[hdr..], version)
}

/// The stream-id bulk string of an XADD one-shot reply (last line).
fn xadd_id(reply: &[u8]) -> Vec<u8> {
    reply
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty() && *l.last().unwrap() == b'\r')
        .map(|l| &l[..l.len() - 1])
        .next_back()
        .unwrap()
        .to_vec()
}

async fn xadd(resp: &str, stream: &str, key: &str, val: &str) {
    resp_one_shot(
        resp,
        &[
            b"XADD",
            stream.as_bytes(),
            b"*",
            key.as_bytes(),
            val.as_bytes(),
        ],
    )
    .await;
}

#[tokio::test]
async fn fetch_versions_ladder_errors_and_hwm() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-fetch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    for i in 1..=3 {
        xadd(&resp, "f1/q0", &format!("k{i}"), &format!("v{i}")).await;
    }
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // v0: plain partition header + records.
    let o = fetch_round(
        &mut sock,
        101,
        0,
        &fetch_body(0, 0, 1, 0, "f1", 0, 0, 1 << 20),
    )
    .await;
    assert_eq!(o.error, 0);
    assert_eq!(o.hwm, 3);
    assert_eq!(batch_head(&o.records), (0, 3), "3 records from offset 0");
    assert!(contains_bytes(&o.records, b"v1"), "value round-trips");

    // v1 mid-log: base_offset echoes the requested offset.
    let o = fetch_round(
        &mut sock,
        102,
        1,
        &fetch_body(1, 0, 1, 0, "f1", 0, 2, 1 << 20),
    )
    .await;
    assert_eq!(o.error, 0);
    assert_eq!(batch_head(&o.records), (2, 1));

    // v4 ladder: throttle, last_stable_offset = hwm, aborted null.
    let o = fetch_round(
        &mut sock,
        103,
        4,
        &fetch_body(4, 0, 1, 1 << 20, "f1", 0, 0, 1 << 20),
    )
    .await;
    assert_eq!(
        (o.throttle, o.error, o.hwm, o.lso, o.aborted_null),
        (0, 0, 3, 3, true)
    );

    // v10: top-level error 0 + session_id 0 (v7+, error BEFORE session);
    // preferred_read_replica is a v11+ field and must be absent.
    let o = fetch_round(
        &mut sock,
        104,
        10,
        &fetch_body(10, 0, 1, 1 << 20, "f1", 0, 0, 1 << 20),
    )
    .await;
    assert_eq!(
        (o.session_id, o.top_error, o.error, o.preferred_replica),
        (0, 0, 0, -1)
    );
    assert_eq!(batch_head(&o.records), (0, 3));

    // EOF (offset == hwm): error 0, 0-length record set.
    let o = fetch_round(
        &mut sock,
        105,
        0,
        &fetch_body(0, 0, 1, 0, "f1", 0, 3, 1 << 20),
    )
    .await;
    assert_eq!((o.error, o.hwm, o.records.len()), (0, 3, 0));

    // Past the end: OFFSET_OUT_OF_RANGE with hwm.
    let o = fetch_round(
        &mut sock,
        106,
        0,
        &fetch_body(0, 0, 1, 0, "f1", 0, 99, 1 << 20),
    )
    .await;
    assert_eq!((o.error, o.hwm, o.records.len()), (1, 3, 0));

    // Unknown topic: UNKNOWN_TOPIC_OR_PARTITION, hwm -1.
    let o = fetch_round(
        &mut sock,
        107,
        0,
        &fetch_body(0, 0, 1, 0, "nope", 0, 0, 1 << 20),
    )
    .await;
    assert_eq!((o.error, o.hwm, o.records.len()), (3, -1, 0));
}

#[tokio::test]
async fn fetch_budget_truncation() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    for (stream, tag) in [("f2/q0", "a"), ("f2/q1", "b")] {
        for i in 1..=2 {
            xadd(&resp, stream, &format!("k{tag}{i}"), &format!("v{tag}{i}")).await;
        }
    }
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // Partition budget 70: one ~68-byte record fits, the second does not.
    let body = fetch_body(3, 0, 1, 1 << 20, "f2", 0, 0, 70);
    let o = fetch_round(&mut sock, 201, 3, &body).await;
    assert_eq!(o.error, 0);
    assert_eq!(
        batch_head(&o.records),
        (0, 1),
        "partition_max_bytes truncates"
    );

    // Global budget 200 is SOFT: every live partition still gets 1 record.
    let body = fetch_two_parts(3, "f2", &[(0, 0, 1 << 20), (1, 0, 1 << 20)]);
    // v3 response: throttle, topics[topic, partitions[part, error,
    // hwm, records]] (no lso/aborted fields before v4).
    let payload = kafka_round(&mut sock, &kafka_req(API_FETCH, 3, 202, false, &body)).await;
    let mut r = Reader::new(&payload);
    r.i32(); // corr
    let hdr = r.pos();
    let mut r = Reader::new(&payload[hdr..]);
    assert_eq!(r.i32(), Some(0), "throttle");
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(2)));
    let mut served = Vec::new();
    for _ in 0..2 {
        let p = r.i32().expect("partition");
        assert_eq!(r.i16(), Some(0), "no error");
        assert_eq!(r.i64(), Some(2), "hwm");
        let n = r.i32().unwrap() as usize;
        let recs = r.take(n).unwrap().to_vec();
        assert_eq!(recs.len(), n);
        if !recs.is_empty() {
            served.push(p);
        }
    }
    served.sort_unstable();
    assert_eq!(
        served,
        vec![0, 1],
        "each partition gets >=1 record under a soft global budget"
    );
}

#[tokio::test]
async fn fetch_long_poll_wakes_and_expires() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-poll-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    // An EMPTY live partition: XADD then XDEL the returned id.
    let id = xadd_id(&resp_one_shot(&resp, &[b"XADD", b"f2/q0", b"*", b"k", b"v"]).await);
    resp_one_shot(&resp, &[b"XDEL", b"f2/q0", &id]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // Pure deadline expiry: max_wait 500ms, min_bytes 1 on an empty
    // partition -> empty answer, but only after the wait.
    let body = fetch_body(0, 500, 1, 0, "f2", 0, 0, 1 << 20);
    let req = kafka_req(API_FETCH, 0, 301, false, &body);
    let t0 = std::time::Instant::now();
    let payload = kafka_round(&mut sock, &req).await;
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(450),
        "waited the slice"
    );
    let mut r = Reader::new(&payload);
    r.i32();
    let hdr = r.pos();
    let o = fetch_out(&payload[hdr..], 0);
    assert_eq!((o.error, o.hwm, o.records.len()), (0, 0, 0));

    // Long poll woken by a produce: XADD 400ms in delivers immediately.
    let body = fetch_body(0, 8_000, 1, 0, "f2", 0, 0, 1 << 20);
    let req = kafka_req(API_FETCH, 0, 302, false, &body);
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&req);
    sock.write_all(&framed).await.expect("write fetch");
    let waker = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        xadd(&resp, "f2/q0", "wake", "now").await;
    });
    let t0 = std::time::Instant::now();
    let payload = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let mut lenb = [0u8; 4];
        sock.read_exact(&mut lenb).await.expect("read len");
        let mut p = vec![0u8; i32::from_be_bytes(lenb) as usize];
        sock.read_exact(&mut p).await.expect("read body");
        p
    })
    .await
    .expect("long poll answered within max_wait");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(7),
        "woken, not timed out"
    );
    waker.await.expect("waker");
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(302));
    let hdr = r.pos();
    let o = fetch_out(&payload[hdr..], 0);
    assert_eq!(o.error, 0);
    assert_eq!(o.hwm, 1);
    assert_eq!(batch_head(&o.records), (0, 1), "the waking record");
    assert!(contains_bytes(&o.records, b"now"));
}
