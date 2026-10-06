//! Byte-fidelity e2e for Kafka header/field NAMES (plans/
//! 2026-10-06-mq-gap 03: bytes are never lost): a header name the
//! wire carries as arbitrary bytes -- including INVALID UTF-8, which
//! the Kafka protocol nominally forbids but never enforces -- must
//! survive produce -> storage -> Fetch byte-exact on BOTH replay
//! paths:
//! - native: real record headers restored from the stored "h" JSON
//!   (names hex-encoded under the `"x"` key);
//! - envelope fallback: exotic stored pairs keep their names
//!   hex-encoded inside the value JSON `{"fields":[[hex,hex]...]}`.
//!
//! Also pins the `rdb-envelope` collision rule: a genuine user header
//! with that literal name WINS and replays verbatim (no marker).

mod common;
mod kafka_front_common;

use common::contains_bytes;
use kafka_front_common::{kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::frame::{put_array_len, put_i16, put_i32, put_i64, put_string, Reader};
use rdb::kafka::record::{build_batch, parse_batch, BatchRecord, Record};
use tokio::net::TcpStream;

const API_PRODUCE: i16 = 0;
const API_FETCH: i16 = 1;

/// Marker header replayed on envelope-fallback records (mirrors
/// `kafka::fetch_records::ENVELOPE_MARKER`, which is crate-private).
const ENVELOPE_MARKER: &[u8] = b"rdb-envelope";

/// One spawned node + connected kafka socket, ready to talk. `_node`
/// is held (never read) so its Drop kills the process at test end.
struct Front {
    resp: String,
    sock: TcpStream,
    corr: i32,
    _node: kafka_front_common::KafkaNode,
}

async fn spawn_front(tag: &str) -> Front {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-fid-{tag}-{}", std::process::id()));
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
async fn fetch_records(f: &mut Front, topic: &str, part: i32, offset: i64) -> Vec<Record> {
    let (error, _hwm, records) = fetch(f, topic, part, offset).await;
    assert_eq!(error, 0);
    parse_batch(&records).expect("fetched batch parses").records
}

/// The envelope JSON the fallback must emit for `pairs`: names AND
/// values hex-encoded (built from the ORIGINAL bytes, so an equality
/// assert proves the round-trip is byte-exact, not merely lossless).
fn expected_envelope(pairs: &[(&[u8], &[u8])]) -> String {
    let fields: Vec<String> = pairs
        .iter()
        .map(|(n, v)| format!("[\"{}\",\"{}\"]", hex::encode(n), hex::encode(v)))
        .collect();
    format!("{{\"fields\":[{}]}}", fields.join(","))
}

#[tokio::test]
async fn native_header_names_round_trip_byte_exact() {
    let mut f = spawn_front("native").await;
    seed_stream(&f.resp).await;
    // Names the protocol calls UTF-8 strings but the wire never
    // validates: invalid-UTF-8, a valid multi-byte sequence, plain
    // ASCII; null and binary values; duplicate names in order.
    let headers: Vec<(Vec<u8>, Option<Vec<u8>>)> = vec![
        (b"\xffname".to_vec(), Some(b"\x00\xff".to_vec())),
        (b"a\xffb".to_vec(), None),
        (b"\xe2\x82\xacuro".to_vec(), Some(Vec::new())),
        (b"a\xffb".to_vec(), Some(b"dup!".to_vec())),
    ];
    let wire: Vec<(&[u8], Option<&[u8]>)> = headers
        .iter()
        .map(|(n, v)| (n.as_slice(), v.as_deref()))
        .collect();
    let batch = build_batch(
        0,
        0,
        &[BatchRecord {
            timestamp_delta: 0,
            key: Some(b"K"),
            value: Some(b"V"),
            headers: wire,
        }],
    );
    let base = produce(&mut f, "t1", 0, &batch).await;
    assert_eq!(base, 1, "produced after the seed entry");

    // Storage side: the "h" pair hex-encodes the names (spot-check
    // the first two; \xffname -> 6e... prefixed by ff).
    let xrange = resp_one_shot(&f.resp, &[b"XRANGE", b"t1/q0", b"-", b"+"]).await;
    assert!(
        contains_bytes(
            &xrange,
            br#"[{"x":"ff6e616d65","v":"00ff"},{"x":"61ff62","v":null}"#.as_slice()
        ),
        "hex names in the stored h JSON: {xrange:?}"
    );

    // Replay side: byte-exact names, values, nulls and order.
    let recs = fetch_records(&mut f, "t1", 0, 1).await;
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].key.as_deref(), Some(b"K".as_slice()));
    assert_eq!(recs[0].value.as_deref(), Some(b"V".as_slice()));
    assert_eq!(recs[0].headers, headers, "names byte-exact, order kept");
}

#[tokio::test]
async fn envelope_fallback_field_names_round_trip_byte_exact() {
    let mut f = spawn_front("env").await;
    seed_stream(&f.resp).await;
    // Exotic stored pairs (3 pairs, no legal "h"): non-UTF-8 field
    // NAMES with binary values, laid down over RESP like legacy or
    // hand-written storage would be.
    let pairs: &[(&[u8], &[u8])] = &[
        (b"a\xffb", b"\x01\x02"),
        (b"\xfez", b""),
        (b"\xffdup", b"t\x80"),
    ];
    resp_one_shot(
        &f.resp,
        &[
            b"XADD",
            b"t1/q0",
            b"*",
            b"a\xffb",
            b"\x01\x02",
            b"\xfez",
            b"",
            b"\xffdup",
            b"t\x80",
        ],
    )
    .await;

    let recs = fetch_records(&mut f, "t1", 0, 0).await;
    assert_eq!(recs.len(), 2);
    let rec = &recs[1];
    assert_eq!(rec.key, None, "key rides inside the envelope");
    assert_eq!(
        rec.headers,
        vec![(ENVELOPE_MARKER.to_vec(), None)],
        "marker header, null value"
    );
    // The envelope carries every original name/value byte: build the
    // expected JSON FROM the original bytes (hex) and demand equality.
    let value = rec.value.as_deref().expect("envelope value");
    let expected = expected_envelope(pairs);
    assert_eq!(
        value,
        expected.as_bytes(),
        "names hex-encoded: byte-exact, no lossy U+FFFD, no collisions"
    );
}

#[tokio::test]
async fn user_rdb_envelope_header_wins() {
    let mut f = spawn_front("coll").await;
    seed_stream(&f.resp).await;
    // A record whose user header is literally named `rdb-envelope`
    // (null value, exactly the marker's shape): the user header wins
    // -- it replays verbatim and the fallback marker is NEVER added
    // (the marker is synthesized only for non-produce shapes).
    let batch = build_batch(
        0,
        0,
        &[
            BatchRecord {
                timestamp_delta: 0,
                key: Some(b"K"),
                value: None,
                headers: vec![(ENVELOPE_MARKER, None)],
            },
            BatchRecord {
                timestamp_delta: 1,
                key: None,
                value: Some(b"v"),
                headers: vec![(ENVELOPE_MARKER, Some(&b"opaque"[..])), (b"other", None)],
            },
        ],
    );
    let base = produce(&mut f, "t1", 0, &batch).await;
    assert_eq!(base, 1);

    let recs = fetch_records(&mut f, "t1", 0, 1).await;
    assert_eq!(recs.len(), 2);
    // Tombstone + the exact marker-shaped user header: replayed as-is.
    assert_eq!(recs[0].key.as_deref(), Some(b"K".as_slice()));
    assert_eq!(recs[0].value, None);
    assert_eq!(recs[0].headers, vec![(ENVELOPE_MARKER.to_vec(), None)]);
    // Valued user header among others: all replay verbatim, in order.
    assert_eq!(
        recs[1].headers,
        vec![
            (ENVELOPE_MARKER.to_vec(), Some(b"opaque".to_vec())),
            (b"other".to_vec(), None),
        ]
    );
}
