//! Batch-1 e2e for the Kafka headers replay fix (plans/2026-10-06-mq-gap
//! 03 scope one; 06 checklist 2.4): REAL record headers survive
//! produce -> Lite field pairs -> Fetch (RecordBatch v2), tombstones
//! keep their headers, header-less messages are byte-identical to
//! before, and every storage shape the produce writer cannot have
//! written falls back to the JSON envelope + `rdb-envelope` marker
//! header instead of guessing.

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::{kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::frame::{put_array_len, put_i16, put_i32, put_i64, put_string, Reader};
use rdb::kafka::record::{build_batch, parse_batch, BatchRecord};
use tokio::net::TcpStream;

const API_PRODUCE: i16 = 0;
const API_FETCH: i16 = 1;

/// Marker header replayed on envelope-fallback records (mirrors
/// `kafka::fetch_records::ENVELOPE_MARKER`, which is crate-private).
const ENVELOPE_MARKER: &str = "rdb-envelope";

/// One spawned node + connected kafka socket, ready to talk. `_node`
/// is held (never read) so its Drop kills the process at test end.
struct Front {
    resp: String,
    sock: TcpStream,
    corr: i32,
    _node: kafka_front_common::KafkaNode,
}

async fn spawn_front(tag: &str) -> Front {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-hdr-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let sock = TcpStream::connect(&kafka).await.expect("connect kafka");
    Front {
        resp,
        sock,
        corr: 0,
        _node: node,
    }
}

/// Create `t1/q0` with one seed entry (this front never auto-creates
/// partitions; the seed also pins offset 0 so produced records start
/// at a known offset).
async fn seed_stream(resp: &str) {
    resp_one_shot(resp, &[b"XADD", b"t1/q0", b"*", b"seed", b"1"]).await;
}

/// Produce v2 one partition onto `f`'s connection; asserts error 0,
/// returns base_offset.
async fn produce(f: &mut Front, topic: &str, part: i32, batch: &[u8]) -> i64 {
    f.corr += 1;
    let mut body = Vec::new();
    put_i16(&mut body, 1); // acks
    put_i32(&mut body, 5_000); // timeout_ms
    put_array_len(&mut body, 1);
    put_string(&mut body, topic);
    put_array_len(&mut body, 1);
    put_i32(&mut body, part);
    put_i32(&mut body, batch.len() as i32);
    body.extend_from_slice(batch);
    let payload = kafka_round(
        &mut f.sock,
        &kafka_req(API_PRODUCE, 2, f.corr, false, &body),
    )
    .await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(f.corr), "correlation id echo");
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(part));
    assert_eq!(r.i16(), Some(0), "produce error 0");
    r.i64().unwrap() // base_offset
}

/// Fetch v4 one partition through `f`: (error, hwm, records blob).
async fn fetch(f: &mut Front, topic: &str, part: i32, offset: i64) -> (i16, i64, Vec<u8>) {
    f.corr += 1;
    let mut body = Vec::new();
    put_i32(&mut body, -1); // replica_id
    put_i32(&mut body, 0); // max_wait_ms
    put_i32(&mut body, 1); // min_bytes
    put_i32(&mut body, 1 << 20); // max_bytes
    body.push(0); // isolation_level
    put_array_len(&mut body, 1);
    put_string(&mut body, topic);
    put_array_len(&mut body, 1);
    put_i32(&mut body, part);
    put_i64(&mut body, offset);
    put_i32(&mut body, 1 << 20); // partition_max_bytes
    let payload = kafka_round(&mut f.sock, &kafka_req(API_FETCH, 4, f.corr, false, &body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(f.corr), "correlation id echo");
    let hdr = r.pos();
    let mut row = Reader::new(&payload[hdr..]);
    assert_eq!(row.i32(), Some(0), "throttle_time_ms");
    assert_eq!(row.array_len(), Some(Some(1)));
    row.string();
    assert_eq!(row.array_len(), Some(Some(1)));
    assert_eq!(row.i32(), Some(part));
    let error = row.i16().unwrap();
    let hwm = row.i64().unwrap();
    row.i64(); // last_stable_offset
    assert_eq!(row.array_len(), Some(None), "aborted_transactions null");
    let records = row.bytes().unwrap().unwrap().to_vec();
    (error, hwm, records)
}

/// Fetch + parse_batch: the decoded records of one partition.
async fn fetch_records(
    f: &mut Front,
    topic: &str,
    part: i32,
    offset: i64,
) -> rdb::kafka::record::RecordBatch {
    let (error, _hwm, records) = fetch(f, topic, part, offset).await;
    assert_eq!(error, 0);
    parse_batch(&records).expect("fetched batch parses")
}

#[tokio::test]
async fn real_headers_roundtrip() {
    let mut f = spawn_front("rt").await;
    seed_stream(&f.resp).await;
    let batch = build_batch(
        0,
        0,
        &[
            BatchRecord {
                timestamp_delta: 0,
                key: None,
                value: Some(b"v0"),
                headers: vec![
                    (&b"null-hdr"[..], None),
                    (&b"trace"[..], Some(&b"\x00\x01\xff"[..])),
                    (&b"z-last"[..], Some(&b"tail"[..])),
                ],
            },
            BatchRecord {
                timestamp_delta: 4,
                key: Some(b"K1"),
                value: Some(&[0xde, 0xad, 0xbe, 0xef]),
                headers: vec![
                    (&b"bin"[..], Some(&b"\xc3\x28"[..])),
                    (&b"dup"[..], None),
                    (&b"dup"[..], Some(&b"2"[..])),
                ],
            },
        ],
    );
    let base = produce(&mut f, "t1", 0, &batch).await;
    assert_eq!(base, 1, "produced after the seed entry");

    let b = fetch_records(&mut f, "t1", 0, 1).await;
    assert_eq!(b.records.len(), 2);
    assert_eq!(b.base_offset, 1, "base_offset = fetch offset");
    // offset/timestamp framing intact. Produce stores arrival-clock ids
    // (client timestamps are dropped), so both records of one append
    // share the batch's first ms: delta 0, first_timestamp = arrival.
    assert_eq!(
        (b.records[0].offset_delta, b.records[1].offset_delta),
        (0, 1)
    );
    assert_eq!(
        (b.records[0].timestamp_delta, b.records[1].timestamp_delta),
        (0, 0)
    );
    assert!(b.first_timestamp > 0);
    // Record 0: null-key, null-value header, binary value, order kept.
    assert_eq!(b.records[0].key, None);
    assert_eq!(b.records[0].value.as_deref(), Some(b"v0".as_slice()));
    assert_eq!(
        b.records[0].headers,
        vec![
            (b"null-hdr".to_vec(), None),
            (b"trace".to_vec(), Some(vec![0x00, 0x01, 0xff])),
            (b"z-last".to_vec(), Some(b"tail".to_vec())),
        ],
        "headers echo in order, no value envelope"
    );
    // Record 1: key + binary value + duplicate names preserved.
    assert_eq!(b.records[1].key.as_deref(), Some(b"K1".as_slice()));
    assert_eq!(b.records[1].value, Some(vec![0xde, 0xad, 0xbe, 0xef]));
    assert_eq!(
        b.records[1].headers,
        vec![
            (b"bin".to_vec(), Some(vec![0xc3, 0x28])),
            (b"dup".to_vec(), None),
            (b"dup".to_vec(), Some(b"2".to_vec())),
        ]
    );
}

#[tokio::test]
async fn tombstone_with_headers() {
    let mut f = spawn_front("ts").await;
    seed_stream(&f.resp).await;
    let batch = build_batch(
        0,
        0,
        &[
            BatchRecord {
                timestamp_delta: 0,
                key: Some(b"tk"),
                value: None,
                headers: vec![(&b"th"[..], None)],
            },
            BatchRecord {
                timestamp_delta: 1,
                key: None,
                value: None,
                headers: vec![(&b"nh"[..], Some(&b"\x00"[..]))],
            },
        ],
    );
    let base = produce(&mut f, "t1", 0, &batch).await;
    assert_eq!(base, 1);
    let b = fetch_records(&mut f, "t1", 0, 1).await;
    assert_eq!(b.records.len(), 2);
    // key'd tombstone: value null (not empty bytes), headers intact.
    assert_eq!(b.records[0].key.as_deref(), Some(b"tk".as_slice()));
    assert_eq!(b.records[0].value, None, "tombstone stays null");
    assert_eq!(b.records[0].headers, vec![(b"th".to_vec(), None)]);
    // null-null + headers: same rule via the __null__ sentinel pair.
    assert_eq!(b.records[1].key, None);
    assert_eq!(b.records[1].value, None);
    assert_eq!(
        b.records[1].headers,
        vec![(b"nh".to_vec(), Some(vec![0x00]))]
    );
}

#[tokio::test]
async fn no_headers_unchanged() {
    let mut f = spawn_front("nh").await;
    seed_stream(&f.resp).await;
    let batch = build_batch(
        0,
        0,
        &[
            BatchRecord {
                timestamp_delta: 0,
                key: Some(b"k"),
                value: Some(b"v"),
                headers: vec![],
            },
            BatchRecord {
                timestamp_delta: 1,
                key: None,
                value: Some(b"value-only"),
                headers: vec![],
            },
            BatchRecord {
                timestamp_delta: 2,
                key: Some(b"t"),
                value: None,
                headers: vec![],
            },
            // An EMPTY headers list is stored exactly like no headers.
            BatchRecord {
                timestamp_delta: 3,
                key: Some(b"e"),
                value: Some(b"empty==absent"),
                headers: vec![],
            },
        ],
    );
    let base = produce(&mut f, "t1", 0, &batch).await;
    assert_eq!(base, 1);
    let b = fetch_records(&mut f, "t1", 0, 1).await;
    assert_eq!(b.records.len(), 4);
    let expect = [
        (Some(&b"k"[..]), Some(&b"v"[..])),
        (None, Some(&b"value-only"[..])),
        (Some(&b"t"[..]), None),
        (Some(&b"e"[..]), Some(&b"empty==absent"[..])),
    ];
    for (rec, (k, v)) in b.records.iter().zip(expect) {
        assert_eq!(rec.key.as_deref(), k);
        assert_eq!(rec.value.as_deref(), v);
        assert_eq!(rec.headers, vec![], "headerCount 0, no marker");
    }
}

#[tokio::test]
async fn exotic_shapes_fall_back_to_envelope() {
    let mut f = spawn_front("ex").await;
    // Hand-written storage shapes the produce writer never emits.
    resp_one_shot(
        &f.resp,
        &[b"XADD", b"t1/q0", b"*", b"a", b"1", b"b", b"", b"c", b"3"],
    )
    .await; // (a) 3 pairs, no h
    resp_one_shot(
        &f.resp,
        &[b"XADD", b"t1/q0", b"*", b"h", b"not-json", b"v", b"V"],
    )
    .await; // (b) h with a non-JSON value
    let json = br#"[{"n":"e","v":null}]"#;
    resp_one_shot(
        &f.resp,
        &[
            b"XADD", b"t1/q0", b"*", b"k", b"K", b"v", b"V", b"h", json, b"x", b"?",
        ],
    )
    .await; // (c) 4 pairs incl. a LEGAL h
            // A normal produced record rides the same stream unharmed.
    let batch = build_batch(
        0,
        0,
        &[BatchRecord {
            timestamp_delta: 0,
            key: Some(b"pk"),
            value: Some(b"pv"),
            headers: vec![(&b"ph"[..], Some(&b"\x01"[..]))],
        }],
    );
    let base = produce(&mut f, "t1", 0, &batch).await;
    assert_eq!(base, 3, "produced after the 3 exotic entries");

    let b = fetch_records(&mut f, "t1", 0, 0).await;
    assert_eq!(b.records.len(), 4);
    let marker = vec![(ENVELOPE_MARKER.as_bytes().to_vec(), None)];
    for (i, rec) in b.records[..3].iter().enumerate() {
        assert_eq!(rec.headers, marker, "exotic {i} carries the marker");
        assert_eq!(rec.key, None, "exotic {i} key rides in the envelope");
        let v = rec.value.as_deref().unwrap_or(&[]);
        assert!(contains_bytes(v, b"\"fields\""), "exotic {i}: {v:?}");
    }
    // Every stored field name/value byte survives inside the envelope.
    assert!(contains_bytes(
        b.records[0].value.as_deref().unwrap(),
        &br#"{"fields":[["61","31"],["62",""],["63","33"]]}"#[..]
    ));
    assert!(contains_bytes(
        b.records[1].value.as_deref().unwrap(),
        &br#"[["68","6e6f742d6a736f6e"],["76","56"]]"#[..]
    ));
    assert!(contains_bytes(
        b.records[2].value.as_deref().unwrap(),
        &br#"[["6b","4b"],["76","56"],["68","5b7b226e223a2265222c2276223a6e756c6c7d5d"],["78","3f"]]"#
            [..]
    ));
    // The mixed-in produced record is NOT degraded.
    let p = &b.records[3];
    assert_eq!(p.key.as_deref(), Some(b"pk".as_slice()));
    assert_eq!(p.value.as_deref(), Some(b"pv".as_slice()));
    assert_eq!(p.headers, vec![(b"ph".to_vec(), Some(vec![0x01]))]);
}
