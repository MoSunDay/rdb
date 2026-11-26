//! Multi-store scenario #157 -- an IoT telemetry collector's long-poll Fetch: the
//! gateway parks a Fetch v0 (max_wait 1000, min_bytes 1) on the EMPTY `telemetry`
//! p0 and it really idles the whole budget (no records, hwm 0); a reader parked
//! BEFORE the next produce wakes on it (200ms in, far under the 5s deadline, base
//! offset 0); `hwm + 5` answers OFFSET_OUT_OF_RANGE 1; ListOffsets v5 -2/-1/by-
//! arrival-ms/future line up with the RESP ids; a Produce acks=0 frame draws NO
//! reply yet lands (same socket keeps answering); an XADD on the bare topic lands
//! in `telemetry/p0` as the 4th record: XLEN == hwm == ListOffsets latest == the
//! MySQL `telemetry_lag` COUNT(*), and the Fetch from 0 returns all 4.
//! Kafka wire + RESP stream + MySQL. One REAL node, fronts kafka + resp + mysql.
//! Deviation: min_bytes is only honored for the all-empty case (src/kafka/fetch.rs:23-29);
//! acks=0 is fire-and-forget, no response frame (src/kafka/produce.rs:17-19).

use std::time::{Duration, Instant};

use crate::common;
use crate::kafka_front_common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use kafka_front_common::topics::create_topics;
use kafka_front_common::{kafka_req, kafka_round};
use rdb::kafka::frame::{Reader, put_array_len, put_bytes, put_i16, put_i32, put_i64, put_string};
use rdb::kafka::record::{BatchRecord, build_batch, parse_batch};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{FrontSpec, bulk_at, front_world, int_of, r, stream_ids};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const TAG: &str = "kpoll157";
const TOPIC: &str = "telemetry";
const STREAM: &str = "telemetry/p0";
const API_PRODUCE: i16 = 0;
const API_FETCH: i16 = 1;
const API_LIST_OFFSETS: i16 = 2;
// Kafka error codes pinned by this scenario (local consts).
const NONE: i16 = 0;
const OFFSET_OUT_OF_RANGE: i16 = 1;
const IDLE_WAIT_MS: i32 = 1_000;

/// One keyed reading of the telemetry log.
fn rec<'a>(key: &'a [u8], value: &'a [u8]) -> BatchRecord<'a> {
    BatchRecord { timestamp_delta: 0, key: Some(key), value: Some(value), headers: vec![] }
}

/// Full-drain reply text with a short quiet gap (array replies).
async fn drain(a: &str, args: &[&[u8]]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 20, 10_000).await)
}

/// Fetch v0 body for `telemetry` p0 at `offset`, min_bytes 1 (one topic,
/// one partition).
fn fetch_body(max_wait: i32, offset: i64) -> Vec<u8> {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    put_i32(&mut b, max_wait);
    put_i32(&mut b, 1); // min_bytes
    put_array_len(&mut b, 1);
    put_string(&mut b, TOPIC);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0); // partition
    put_i64(&mut b, offset);
    put_i32(&mut b, 1 << 20); // partition max_bytes
    b
}

/// Decode a Fetch v0 reply (corr checked): (error, hwm, record-set bytes).
fn fetch_out(payload: &[u8], corr: i32) -> (i16, i64, Vec<u8>) {
    let mut r = Reader::new(payload);
    assert_eq!(r.i32(), Some(corr), "correlation id echo");
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)), "one topic");
    assert_eq!(r.string().as_deref(), Some(TOPIC));
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    assert_eq!(r.i32(), Some(0), "partition 0");
    let err = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    let n = r.i32().unwrap_or(-1) as usize;
    let records = r.take(n).unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0, "v0 reply fully drained");
    (err, hwm, records)
}

/// Park one long-poll Fetch v0 on its OWN connection (the task owns the
/// socket); the decoded reply rides the oneshot.
fn spawn_poll(addr: &str, corr: i32, max_wait: i32, offset: i64) -> oneshot::Receiver<(i16, i64, Vec<u8>)> {
    let (tx, rx) = oneshot::channel();
    let addr = addr.to_string();
    tokio::spawn(async move {
        let mut sock = TcpStream::connect(&addr).await.expect("connect kafka front");
        let req = kafka_req(API_FETCH, 0, corr, false, &fetch_body(max_wait, offset));
        let payload = kafka_round(&mut sock, &req).await;
        let _ = tx.send(fetch_out(&payload, corr));
    });
    rx
}

/// A parked reader's reply, bounded so a lost wake fails loudly.
async fn answered(rx: oneshot::Receiver<(i16, i64, Vec<u8>)>) -> (i16, i64, Vec<u8>) {
    tokio::time::timeout(Duration::from_secs(8), rx)
        .await
        .expect("parked fetch answered within 8s")
        .expect("reader task alive")
}

/// ListOffsets v5 for p0 by `ts`: (corr echo, error, timestamp, offset).
async fn list_offsets(sock: &mut TcpStream, corr: i32, ts: i64) -> (i32, i16, i64, i64) {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    b.push(0u8); // isolation_level
    put_array_len(&mut b, 1);
    put_string(&mut b, TOPIC);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_i32(&mut b, -1); // current_leader_epoch
    put_i64(&mut b, ts);
    let payload = kafka_round(sock, &kafka_req(API_LIST_OFFSETS, 5, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    let echo = r.i32().unwrap();
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.i32(), Some(0), "throttle first");
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.string().as_deref(), Some(TOPIC));
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0), "partition echo");
    let err = r.i16().unwrap();
    let ts_out = r.i64().unwrap();
    let off = r.i64().unwrap();
    assert_eq!(r.i32(), Some(-1), "leader_epoch -1 (no epochs)");
    (echo, err, ts_out, off)
}

/// A framed Produce v3 request (`acks` rides the request): the acks=0
/// shape the fixture helper (acks=1, waits for the reply) cannot express.
fn produce_frame(corr: i32, acks: i16, records: &[BatchRecord<'_>]) -> Vec<u8> {
    let batch = build_batch(0, 1_000, records);
    let mut b = Vec::new();
    put_i16(&mut b, -1); // transactional_id: null
    put_i16(&mut b, acks);
    put_i32(&mut b, 5_000); // timeout_ms
    put_array_len(&mut b, 1);
    put_string(&mut b, TOPIC);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_bytes(&mut b, &batch);
    let req = kafka_req(API_PRODUCE, 3, corr, false, &b);
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&req);
    framed
}

/// Arrival ms of a stream id (`ms-seq`).
fn id_ms(id: &str) -> i64 {
    id.split_once('-').expect("ms-seq id").0.parse().expect("ms digits")
}

#[tokio::test]
async fn long_poll_fetch_parks_until_produce() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { kafka: true, mysql: true, ..Default::default() };
    let (front, a) = front_world(TAG, &spec).await;
    let mut c = connect_root(&front.node).await;
    let mut sock = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    let mut corr = 1;
    assert_eq!(create_topics(&mut sock, corr, TOPIC, 1, -1, false).await, NONE, "create telemetry");
    corr += 1;
    assert_eq!(int_of(&r(&a, &[b"XLEN", STREAM.as_bytes()]).await), 0, "p0 live and empty");

    // a) An idle partition: the collector's poll (max_wait 1000, min_bytes
    //    1) really parks the whole budget before answering empty.
    let reader = spawn_poll(&front.kafka, corr, IDLE_WAIT_MS, 0);
    corr += 1;
    let t_idle = scenario_common::perf::now();
    let (err, hwm, records) = answered(reader).await;
    scenario_common::perf::at_least("kpoll157/idle-park", t_idle, Duration::from_millis(900));
    scenario_common::perf::within("kpoll157/idle-park-ceiling", t_idle, Duration::from_secs(4));
    assert_eq!((err, hwm, records.len()), (NONE, 0, 0), "idle poll answers empty");

    // b) Hot block: a reader parked BEFORE the produce wakes on it, then
    //    the resume algebra around the log it produced.
    let t = scenario_common::perf::now();
    let reader = spawn_poll(&front.kafka, corr, 5_000, 0);
    corr += 1;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let base = kafka_produce_v3(&front.kafka, corr, TOPIC, &[rec(b"dev-7", b"temp=21")]).await;
    corr += 1;
    let (werr, whwm, wrecords) = answered(reader).await;
    scenario_common::perf::within("kpoll157/parked-fetch-wake", t, Duration::from_secs(2));
    // c) Out of range: hwm + 5 answers OFFSET_OUT_OF_RANGE, hwm = log end.
    let payload = kafka_round(&mut sock, &kafka_req(API_FETCH, 0, corr, false, &fetch_body(0, 6))).await;
    corr += 1;
    let oor = fetch_out(&payload, corr - 1);
    // d) A second reading 40ms later; the by-timestamp resume view must
    //    line up with the RESP ids of the same two entries.
    tokio::time::sleep(Duration::from_millis(40)).await;
    let base2 = kafka_produce_v3(&front.kafka, corr, TOPIC, &[rec(b"dev-8", b"temp=22")]).await;
    corr += 1;
    let ids = stream_ids(&drain(&a, &[b"XRANGE", STREAM.as_bytes(), b"-", b"+"]).await);
    assert_eq!(ids.len(), 2, "{ids:?}");
    let (ms0, ms1) = (id_ms(&ids[0]), id_ms(&ids[1]));
    let mut offsets = Vec::new();
    for ts in [-2, -1, 0, ms0, ms0 + 1, ms1 + 3_600_000] {
        let want = corr;
        corr += 1;
        let (echo, err, ts_out, off) = list_offsets(&mut sock, want, ts).await;
        assert_eq!(echo, want, "correlation echo");
        offsets.push((err, ts_out, off));
    }
    // e) Produce acks=0: NO reply frame, yet the record lands and the
    //    same socket keeps answering later requests.
    let mut fire = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    fire.write_all(&produce_frame(corr, 0, &[rec(b"dev-9", b"temp=23")])).await.expect("write acks=0 produce");
    corr += 1;
    let mut byte = [0u8; 1];
    let silent = tokio::time::timeout(Duration::from_millis(500), fire.read(&mut byte)).await;
    assert!(silent.is_err(), "acks=0 must not answer: {silent:?}");
    let deadline = Instant::now() + Duration::from_secs(3);
    let latest = loop {
        let want = corr;
        corr += 1;
        let (echo, err, _ts, off) = list_offsets(&mut fire, want, -1).await;
        assert_eq!((echo, err), (want, NONE), "the acks=0 socket still answers");
        if off == 3 {
            break off;
        }
        assert!(Instant::now() < deadline, "acks=0 record never landed (len {off})");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // f) The RESP side appends the 4th reading: two fields map
    //    positionally to (key, value) on Fetch (fetch_records.rs:158-165).
    let reply = drain(&a, &[b"XADD", TOPIC.as_bytes(), b"*", b"temp", b"cellar-24", b"site", b"rack-3"]).await;
    let xlen = int_of(&r(&a, &[b"XLEN", STREAM.as_bytes()]).await);
    let payload = kafka_round(&mut sock, &kafka_req(API_FETCH, 0, corr, false, &fetch_body(0, 0))).await;
    let (err, hwm, records) = fetch_out(&payload, corr);
    scenario_common::perf::within("kpoll157/long-poll-cycle", t, Duration::from_secs(3));

    // g) The answers: the waking record, the resume algebra, the acks=0
    //    landing, the RESP-appended tail and the full log.
    assert_eq!(base, 0, "the first reading takes ordinal 0");
    assert_eq!(base2, 1, "the second takes ordinal 1");
    let woke = parse_batch(&wrecords).expect("one waking batch");
    assert_eq!((werr, whwm, woke.base_offset, woke.records.len()), (NONE, 1, 0, 1), "woke with exactly the new reading");
    assert_eq!((woke.records[0].key.as_deref(), woke.records[0].value.as_deref()), (Some(b"dev-7".as_slice()), Some(b"temp=21".as_slice())), "the waking record");
    assert_eq!(latest, 3, "acks=0 record landed as ordinal 2");
    assert!(ms1 >= ms0 + 40, "40ms gap between the readings: {ids:?}");
    assert_eq!(
        offsets,
        vec![(NONE, -1, 0), (NONE, -1, 2), (NONE, ms0, 0), (NONE, ms0, 0), (NONE, ms1, 1), (NONE, -1, -1)],
        "-2 earliest, -1 latest, 0 / ms0 first, ms0+1 second, future none"
    );
    assert_eq!(oor, (OFFSET_OUT_OF_RANGE, 1, Vec::new()), "hwm + 5 is out of range, hwm = log end");
    assert!(reply.starts_with("*2\r\n") && bulk_at(reply.as_bytes(), 0) == STREAM, "XADD picks the only child: {reply}");
    let full = parse_batch(&records).expect("one batch from 0");
    assert_eq!((xlen, err, hwm, full.base_offset, full.records.len()), (4, NONE, 4, 0, 4), "XLEN == hwm == log");
    let values: Vec<&[u8]> = full.records.iter().map(|r| r.value.as_deref().unwrap_or(b"")).collect();
    assert_eq!(values, vec![&b"temp=21"[..], &b"temp=22"[..], &b"temp=23"[..], &b"rack-3"[..]], "the log in order");
    assert_eq!((full.records[0].key.as_deref(), full.records[3].key.as_deref()), (Some(b"dev-7".as_slice()), Some(b"cellar-24".as_slice())), "produce key vs the 2-field XADD's positional key");

    // h) The MySQL lag ledger: one row per reading, the stream depth is
    //    the ledger's COUNT(*) and every reading sits under the same hwm.
    ddl(&mut c, "CREATE TABLE telemetry_lag (reading INT NOT NULL PRIMARY KEY, source VARCHAR(8) NOT NULL, log_offset BIGINT NOT NULL, hwm BIGINT NOT NULL, lag BIGINT NOT NULL)").await;
    let lag_sum: i64 = (0..4).map(|o| hwm - o).sum();
    for (i, src) in ["poll", "poll", "acks0", "xadd"].iter().enumerate() {
        run(&mut c, &format!("INSERT INTO telemetry_lag VALUES ({}, '{src}', {i}, {hwm}, {})", i + 1, hwm - i as i64)).await;
    }
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM telemetry_lag").await, vec![xlen], "COUNT(*) == XLEN");
    assert_eq!(ints(&mut c, "SELECT MAX(hwm) FROM telemetry_lag").await, vec![hwm], "every row under the live hwm");
    assert_eq!(ints(&mut c, "SELECT SUM(lag) FROM telemetry_lag").await, vec![lag_sum], "the lag ledger sums to 0+1+2+3");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM telemetry_lag WHERE source = 'poll' AND log_offset < 2").await, vec![2], "the two polled readings");
    scenario_common::perf::scenario(TAG, t0);
}
