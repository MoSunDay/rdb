//! Produce (0) v2 + ListOffsets (2) v1/v0 over the wire against the
//! REAL binary: RESP-created streams are the topic/partition view, and
//! everything the Kafka front answers is hand-decoded with the
//! production `kafka::frame::Reader`. Error paths (compression, CRC,
//! magic, unknown/invalid topics, bad acks) ride the same connection.

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::{kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::frame::{put_i16, put_i32, put_i64, put_array_len, put_string, Reader};
use rdb::kafka::record::{build_batch, crc32c, BatchRecord};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const API_PRODUCE: i16 = 0;
const API_LIST_OFFSETS: i16 = 2;
const API_API_VERSIONS: i16 = 18;

/// Frame + send one Produce v2 request for `topic`/`part`; `acks=0`
/// callers must NOT wait for a response (the broker sends none).
async fn send_produce(
    sock: &mut TcpStream,
    corr: i32,
    acks: i16,
    topic: &str,
    part: i32,
    batch: &[u8],
) {
    let mut body = Vec::new();
    put_i16(&mut body, acks);
    put_i32(&mut body, 5_000); // timeout_ms
    put_array_len(&mut body, 1);
    put_string(&mut body, topic);
    put_array_len(&mut body, 1);
    put_i32(&mut body, part);
    put_i32(&mut body, batch.len() as i32);
    body.extend_from_slice(batch);
    let req = kafka_req(API_PRODUCE, 2, corr, false, &body);
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&req);
    sock.write_all(&framed).await.expect("write produce");
}

/// Send + read back one Produce v2 response body (past corr id).
async fn produce_round(
    sock: &mut TcpStream,
    corr: i32,
    acks: i16,
    topic: &str,
    part: i32,
    batch: &[u8],
) -> Vec<u8> {
    send_produce(sock, corr, acks, topic, part, batch).await;
    let payload = read_frame(sock).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation id echo");
    let hdr = r.pos();
    payload[hdr..].to_vec()
}

/// Read one length-framed response payload off the socket.
async fn read_frame(sock: &mut TcpStream) -> Vec<u8> {
    let mut lenb = [0u8; 4];
    sock.read_exact(&mut lenb).await.expect("read kafka len");
    let len = i32::from_be_bytes(lenb) as usize;
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload).await.expect("read kafka body");
    payload
}

/// (partition, error, base_offset, log_append_time) of a Produce v2
/// single-partition response.
fn produce_out(body: &[u8]) -> (i32, i16, i64, i64) {
    let mut r = Reader::new(body);
    assert_eq!(r.array_len(), Some(Some(1)), "one topic");
    let _topic = r.string().expect("topic echo");
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    (
        r.i32().unwrap(),
        r.i16().unwrap(),
        r.i64().unwrap(),
        r.i64().unwrap(),
    )
}

/// ListOffsets v1 for one topic; returns (partition, error, ts, offset)
/// per requested partition in order.
async fn list_offsets(
    sock: &mut TcpStream,
    corr: i32,
    version: i16,
    topic: &str,
    parts: &[(i32, i64)],
) -> Vec<(i32, i16, i64, i64)> {
    let mut body = Vec::new();
    put_i32(&mut body, -1); // replica_id
    put_array_len(&mut body, 1);
    put_string(&mut body, topic);
    put_array_len(&mut body, parts.len());
    for (p, ts) in parts {
        put_i32(&mut body, *p);
        put_i64(&mut body, *ts);
    }
    let payload = kafka_round(sock, &kafka_req(API_LIST_OFFSETS, version, corr, false, &body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(topic));
    let n = r.array_len().unwrap().unwrap();
    (0..n)
        .map(|_| {
            if version == 0 {
                let p = r.i32().unwrap();
                let e = r.i16().unwrap();
                let offs: Vec<i64> = match r.array_len().unwrap() {
                    Some(k) => (0..k).map(|_| r.i64().unwrap()).collect(),
                    None => vec![],
                };
                (p, e, -1, offs.first().copied().unwrap_or(-1))
            } else {
                (r.i32().unwrap(), r.i16().unwrap(), r.i64().unwrap(), r.i64().unwrap())
            }
        })
        .collect()
}

/// Patch a batch's attributes and re-seal the CRC (the CRC covers
/// attributes..end).
fn set_attributes(batch: &mut Vec<u8>, attr: i16) {
    batch[21..23].copy_from_slice(&attr.to_be_bytes());
    let crc = crc32c(&batch[21..]);
    batch[17..21].copy_from_slice(&crc.to_be_bytes());
}

#[tokio::test]
async fn produce_appends_and_listoffsets_answer() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-prod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;

    // Topic view: t1/q0 (one RESP-born entry) and t2/p0 (p-suffix queue).
    resp_one_shot(&resp, &[b"XADD", b"t1/q0", b"*", b"hello", b"world"]).await;
    resp_one_shot(&resp, &[b"XADD", b"t2/p0", b"*", b"seed", b"1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // ---- acks=1, two records: key'd and key-less ----
    let batch = build_batch(
        0,
        1_000,
        &[
            BatchRecord { timestamp_delta: 0, key: Some(b"k1"), value: Some(b"v1"), headers: vec![] },
            BatchRecord { timestamp_delta: 5, key: None, value: Some(b"v2"), headers: vec![] },
        ],
    );
    let body = produce_round(&mut sock, 1, 1, "t1", 0, &batch).await;
    assert_eq!(produce_out(&body), (0, 0, 1, -1), "base_offset = pre-append len (1 xadd)");

    // ---- headers + tombstone value ----
    let batch = build_batch(
        0,
        2_000,
        &[BatchRecord {
            timestamp_delta: 0,
            key: Some(b"k2"),
            value: None,
            headers: vec![("h1", None), ("h2", Some(b"\x00\xff"))],
        }],
    );
    let body = produce_round(&mut sock, 2, 1, "t1", 0, &batch).await;
    assert_eq!(produce_out(&body), (0, 0, 3, -1), "second produce continues the log");

    // ---- field pairs visible over RESP ----
    let xrange = resp_one_shot(&resp, &[b"XRANGE", b"t1/q0", b"-", b"+"]).await;
    assert!(contains_bytes(&xrange, b"$1\r\nk\r\n$2\r\nk1\r\n"), "k=k1 pair: {xrange:?}");
    assert!(contains_bytes(&xrange, b"$1\r\nv\r\n$2\r\nv1\r\n"), "v=v1 pair");
    assert!(contains_bytes(&xrange, b"$1\r\nv\r\n$2\r\nv2\r\n"), "key-less record keeps only v");
    assert!(contains_bytes(&xrange, b"$8\r\n__null__\r\n$0\r\n\r\n"), "null value -> __null__ sentinel");
    let json = r#"[{"n":"h1","v":null},{"n":"h2","v":"00ff"}]"#;
    assert!(
        contains_bytes(&xrange, format!("${}\r\n{}\r\n", json.len(), json).as_bytes()),
        "headers JSON pair: {xrange:?}"
    );
    let xlen = resp_one_shot(&resp, &[b"XLEN", b"t1/q0"]).await;
    assert!(contains_bytes(&xlen, b":4\r\n"), "1 xadd + 3 produced: {xlen:?}");

    // ---- p<N> precedence over q<N> for partition 0 ----
    let batch = build_batch(0, 3_000, &[BatchRecord {
        timestamp_delta: 0,
        key: None,
        value: Some(b"into-p0"),
        headers: vec![],
    }]);
    let body = produce_round(&mut sock, 3, 1, "t2", 0, &batch).await;
    assert_eq!(produce_out(&body), (0, 0, 1, -1), "lands on t2/p0, not a q0");
    let xlen = resp_one_shot(&resp, &[b"XLEN", b"t2/p0"]).await;
    assert!(contains_bytes(&xlen, b":2\r\n"), "seed + 1 produced");

    // ---- ListOffsets v1: latest / earliest / by-ts / miss / unknown ----
    let got = list_offsets(
        &mut sock,
        10,
        1,
        "t1",
        &[(0, -1), (0, -2), (0, 0), (0, i64::MAX), (7, -1)],
    )
    .await;
    assert_eq!(got[0], (0, 0, -1, 4), "latest = len");
    assert_eq!(got[1], (0, 0, -1, 0), "earliest = 0");
    assert_eq!(got[2].0, 0);
    assert_eq!(got[2].1, 0);
    assert_eq!(got[2].3, 0, "ts 0 -> first entry");
    assert!(got[2].2 > 1_000_000_000_000, "arrival-clock ms timestamp: {got:?}");
    assert_eq!(got[3], (0, 0, -1, -1), "future ts -> not found");
    assert_eq!(got[4], (7, 3, -1, -1), "unknown partition");

    // ---- ListOffsets v0: offsets int64 array shape ----
    let got = list_offsets(&mut sock, 11, 0, "t1", &[(0, -1), (0, i64::MAX)]).await;
    assert_eq!(got[0], (0, 0, -1, 4), "v0 latest answers one offset");
    assert_eq!(got[1], (0, 0, -1, -1), "v0 miss -> empty offsets array");

    // ---- acks=0: NO response frame; the connection stays usable ----
    let batch = build_batch(0, 4_000, &[BatchRecord {
        timestamp_delta: 0,
        key: None,
        value: Some(b"fire"),
        headers: vec![],
    }]);
    send_produce(&mut sock, 12, 0, "t1", 0, &batch).await;
    // ApiVersions answer proves no produce bytes were sent back.
    let req = kafka_req(API_API_VERSIONS, 0, 300, false, b"");
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&req);
    sock.write_all(&framed).await.expect("write apiversions");
    let payload = read_frame(&mut sock).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(300), "next response is ApiVersions, not a produce ack");
    let xlen = resp_one_shot(&resp, &[b"XLEN", b"t1/q0"]).await;
    assert!(contains_bytes(&xlen, b":5\r\n"), "acks=0 still appends (eventually)");
}

#[tokio::test]
async fn produce_error_paths() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-prod-err-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    resp_one_shot(&resp, &[b"XADD", b"t1/q0", b"*", b"seed", b"1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    let good = |ts: i64| {
        build_batch(ts, ts, &[BatchRecord {
            timestamp_delta: 0,
            key: None,
            value: Some(b"x"),
            headers: vec![],
        }])
    };
    let mut corr = 1;
    macro_rules! expect {
        ($batch:expr, $topic:expr, $part:expr, $code:expr) => {{
            corr += 1;
            let body = produce_round(&mut sock, corr, 1, $topic, $part, &$batch).await;
            assert_eq!(produce_out(&body), ($part, $code, -1, -1), "corr {corr}");
        }};
    }

    // attributes low bits = gzip, CRC re-sealed over (uncompressed)
    // records: without `kafka-codecs` -> 76; with the feature the gzip
    // decoder rejects/garbles the plain bytes -> 2 CORRUPT_MESSAGE.
    let mut compressed = good(1);
    set_attributes(&mut compressed, 1);
    #[cfg(feature = "kafka-codecs")]
    expect!(compressed, "t1", 0, 2);
    #[cfg(not(feature = "kafka-codecs"))]
    expect!(compressed, "t1", 0, 76);

    // CRC corruption (payload byte flipped, stored CRC stale): 2.
    let mut corrupted = good(2);
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xFF;
    expect!(corrupted, "t1", 0, 2);

    // magic != 2: 35.
    let mut wrong_magic = good(3);
    wrong_magic[16] = 3;
    expect!(wrong_magic, "t1", 0, 35);

    // unknown partition / unknown topic: 3.
    expect!(good(4), "t1", 7, 3);
    expect!(good(5), "nope", 0, 3);

    // invalid topic name (space): 17.
    expect!(good(6), "bad name", 0, 17);

    // acks=5 fails every partition in-band: 21, nothing appended.
    let body = produce_round(&mut sock, 99, 5, "t1", 0, &good(7)).await;
    assert_eq!(produce_out(&body), (0, 21, -1, -1), "INVALID_REQUIRED_ACKS");
    let xlen = resp_one_shot(&resp, &[b"XLEN", b"t1/q0"]).await;
    assert!(contains_bytes(&xlen, b":1\r\n"), "no error-path appends: {xlen:?}");
}
