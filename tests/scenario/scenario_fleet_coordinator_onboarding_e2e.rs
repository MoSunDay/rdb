//! Multi-store scenario #143 -- fleet onboarding: a fleet agent bootstraps
//! over the Kafka wire (ApiVersions v0-v3, the error-35 fallback for a
//! flexible-framed unserved v4, FindCoordinator v0/v1 resolving to this
//! very broker), then reports five vehicle status records and a TOMBSTONE
//! (null value) for the decommissioned v3. The RESP stream view of
//! `<topic>/p0`, the Kafka Fetch/ListOffsets view and a MySQL fleet table
//! must agree on which vehicles are still alive.
//! SURFACES: Kafka wire + RESP stream + MySQL. One REAL node, Kafka + RESP + MySQL.
//! Deviation: Kafka record keys have NO string-key (KV) view -- a record lives
//! only as a Lite stream entry in `<topic>/p<N>` (src/kafka/produce.rs:141-146),
//! so the tombstone is asserted on the stream (the `__null__` marker pair,
//! src/kafka/produce.rs:254-256) and `GET <topic>/v3` answers nil.

use crate::common;
use crate::kafka_front_common::topics::list_offsets_v5;
use crate::kafka_front_common::{kafka_req, kafka_round};
use crate::scenario_common;
use crate::scenario_common::kafka::kafka_produce_v3;
use common::mysql::{col, connect_root, ddl, rows, run, s};
use mysql_async::Value as MVal;
use rdb::kafka::frame::{Reader, put_array_len, put_i32, put_i64, put_i8, put_string};
use rdb::kafka::record::{BatchRecord, RecordBatch, parse_batch};
use scenario_common::{FrontSpec, front_world, int_of, r, rb, rf, stream_ids};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::net::TcpStream;

const TAG: &str = "coord143";
const TOPIC: &str = "fleet-coord143";
const GROUP: &str = "fleet-coord143";
const VEHICLES: [&str; 5] = ["v0", "v1", "v2", "v3", "v4"];
const API_VERSIONS: i16 = 18;
const API_FIND_COORDINATOR: i16 = 10;
const API_FETCH: i16 = 1;
const UNSUPPORTED_VERSION: i16 = 35;
const INVALID_REQUEST: i16 = 42;
/// (api_key, min, max) rows the onboarding agent relies on.
const RELIED_ON: [(i16, i16, i16); 5] = [(0, 0, 3), (1, 0, 10), (2, 0, 5), (10, 0, 1), (18, 0, 3)];

/// Status payload the agent reports for vehicle `i` (all alive at first).
fn status_json(i: usize) -> String {
    format!("{{\"state\":\"alive\",\"odo\":{}}}", 1_000 * (i + 1))
}

/// One Produce v3 round (own connection, acks=1): the echoed base offset.
async fn produce(addr: &str, corr: i32, key: &[u8], value: Option<&[u8]>) -> i32 {
    let records = [BatchRecord {
        timestamp_delta: 0,
        key: Some(key),
        value,
        headers: vec![],
    }];
    kafka_produce_v3(addr, corr, TOPIC, &records).await
}

/// Parse an ApiVersions reply body laid out as `version`:
/// (error_code, [(api_key, min_version, max_version)] in body order).
fn parse_api_versions(body: &[u8], version: i16) -> (i16, Vec<(i16, i16, i16)>) {
    let mut r = Reader::new(body);
    let err = r.i16().unwrap();
    let n = if version >= 3 {
        r.compact_array_len().unwrap().unwrap()
    } else {
        r.array_len().unwrap().unwrap()
    };
    let mut apis = Vec::new();
    for _ in 0..n {
        let row = (r.i16().unwrap(), r.i16().unwrap(), r.i16().unwrap());
        if version >= 3 {
            r.skip_tagged_fields().expect("entry tagged fields");
        }
        apis.push(row);
    }
    if version >= 1 {
        assert_eq!(r.i32(), Some(0), "throttle_time_ms after the array");
    }
    if version >= 3 {
        r.skip_tagged_fields().expect("body tagged fields");
    }
    assert_eq!(r.remaining(), 0, "ApiVersions v{version} reply fully drained");
    (err, apis)
}

/// One ApiVersions round; `body_version` is the layout the broker
/// answers with (0 for the unsupported-version fallback, conn.rs:203-211).
async fn api_versions_round(
    sock: &mut TcpStream,
    corr: i32,
    api_key: i16,
    req_version: i16,
    body_version: i16,
) -> (i16, Vec<(i16, i16, i16)>) {
    // KIP-511: every ApiVersions request from v3 on is flexible-framed
    // (src/kafka/mod.rs:160-165) -- INCLUDING an unserved v4, which the
    // broker parses as flexible before answering the v0 fallback (a
    // classically framed v4 is "malformed request header" and the
    // connection drops). Body: compact software name/version + tags.
    let flexible = api_key == API_VERSIONS && req_version >= 3;
    let mut body: Vec<u8> = Vec::new();
    if flexible {
        body.extend_from_slice(&[4, b'e', b'2', b'e', 2, b'1', 0]);
    }
    let payload = kafka_round(sock, &kafka_req(api_key, req_version, corr, flexible, &body)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation echo (classic header)");
    parse_api_versions(&payload[r.pos()..], body_version)
}

/// The advertised table must carry the rows the agent relies on, stay
/// key-sorted and, without `kafka_token`, hold exactly the 20 classic
/// apis (no SaslHandshake/SaslAuthenticate).
fn assert_api_table(apis: &[(i16, i16, i16)]) {
    assert_eq!(apis.len(), 20, "advertised api count: {apis:?}");
    assert!(
        apis.windows(2).all(|w| w[0].0 < w[1].0),
        "key-sorted table: {apis:?}"
    );
    for want in RELIED_ON {
        assert!(apis.contains(&want), "{want:?} advertised: {apis:?}");
    }
    assert!(
        !apis.iter().any(|(k, _, _)| *k == 17 || *k == 36),
        "no SASL pair without kafka_token: {apis:?}"
    );
}

/// FindCoordinator v0/v1 for `key` (ctype 0 = group, 1 = transaction):
/// (error, error_message, node_id, host, port).
async fn find_coordinator(
    sock: &mut TcpStream,
    corr: i32,
    version: i16,
    key: &str,
    ctype: i8,
) -> (i16, Option<String>, i32, String, i32) {
    let mut b = Vec::new();
    put_string(&mut b, key);
    if version >= 1 {
        put_i8(&mut b, ctype);
    }
    let payload = kafka_round(sock, &kafka_req(API_FIND_COORDINATOR, version, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation echo");
    if version >= 1 {
        assert_eq!(r.i32(), Some(0), "throttle_time_ms leads the v1 reply");
    }
    let err = r.i16().unwrap();
    let msg = if version >= 1 {
        r.nullable_string().unwrap()
    } else {
        None
    };
    let node = r.i32().unwrap();
    let host = r.string().unwrap();
    let port = r.i32().unwrap();
    assert_eq!(r.remaining(), 0, "v{version} reply fully drained");
    (err, msg, node, host, port)
}

/// Fetch v0 from `offset` on (topic, p0): (error, hwm, record-set bytes).
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
    assert_eq!(r.i32(), Some(corr), "correlation echo");
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

/// Fold the fetched batch in offset order: every vehicle's LATEST
/// (value, offset) -- a tombstone (null value) overwrites its status.
fn latest_by_key(batch: &RecordBatch) -> Vec<(String, Option<String>, i64)> {
    let mut latest: BTreeMap<String, (Option<String>, i64)> = BTreeMap::new();
    for rec in &batch.records {
        let key = String::from_utf8(rec.key.clone().expect("keyed record")).unwrap();
        let value = rec
            .value
            .clone()
            .map(|v| String::from_utf8(v).expect("utf8 status"));
        latest.insert(key, (value, batch.base_offset + rec.offset_delta as i64));
    }
    latest
        .into_iter()
        .map(|(k, (v, off))| (k, v, off))
        .collect()
}

#[tokio::test]
async fn coordinator_versions_and_tombstones_line_up() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        mysql: true,
        extra_yaml: "kafka_auto_create_topics: true\n".to_string(),
        ..Default::default()
    };
    let (front, a) = front_world(TAG, &spec).await;
    let mut c = connect_root(&front.node).await;
    let kafka_port: i32 = front.kafka.rsplit_once(':').unwrap().1.parse().unwrap();
    ddl(
        &mut c,
        "CREATE TABLE fleet (vehicle VARCHAR(16) PRIMARY KEY, state VARCHAR(8) NOT NULL, \
         offset BIGINT NOT NULL)",
    )
    .await;

    // a) The hot block: onboarding (ApiVersions v0-v3 + the 35 fallback +
    //    an unknown api, FindCoordinator v0/v1) and the six produces.
    let mut sock = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    let t = scenario_common::perf::now();
    let mut corr = 1;
    for v in 0..=3 {
        let (err, apis) = api_versions_round(&mut sock, corr, API_VERSIONS, v, v).await;
        assert_eq!(err, 0, "ApiVersions v{v} error code");
        assert_api_table(&apis);
        corr += 1;
    }
    // Unserved v4 and an unknown api both answer the v0-body fallback
    // with error 35 plus the full table (the client renegotiation path).
    for (key, ver) in [(API_VERSIONS, 4), (999, 0)] {
        let (err, apis) = api_versions_round(&mut sock, corr, key, ver, 0).await;
        assert_eq!(err, UNSUPPORTED_VERSION, "api {key} v{ver}: unsupported");
        assert_api_table(&apis);
        corr += 1;
    }
    let want_node = (0, None, 1, "127.0.0.1".to_string(), kafka_port);
    assert_eq!(
        find_coordinator(&mut sock, corr, 0, GROUP, 0).await,
        want_node,
        "FindCoordinator v0: this broker is the group coordinator"
    );
    corr += 1;
    assert_eq!(
        find_coordinator(&mut sock, corr, 1, GROUP, 0).await,
        want_node,
        "FindCoordinator v1: same node, null error_message"
    );
    corr += 1;
    let tx = find_coordinator(&mut sock, corr, 1, GROUP, 1).await;
    assert_eq!(tx.0, INVALID_REQUEST, "v1 type 1 (transaction) refused");
    assert_eq!(
        (tx.1.as_deref(), tx.2, tx.3.as_str(), tx.4),
        (Some("only group coordination is supported"), -1, "", -1),
        "refusal carries the message and sentinel node/port"
    );
    corr += 1;
    for (i, v) in VEHICLES.iter().enumerate() {
        let base = produce(&front.kafka, corr, v.as_bytes(), Some(status_json(i).as_bytes())).await;
        assert_eq!(base, i as i32, "status record {v} lands at its own offset");
        corr += 1;
        run(
            &mut c,
            &format!("INSERT INTO fleet VALUES ('{v}', 'alive', {i})"),
        )
        .await;
    }
    let base = produce(&front.kafka, corr, b"v3", None).await;
    assert_eq!(base, 5, "the tombstone is the sixth record");
    corr += 1;
    run(
        &mut c,
        "UPDATE fleet SET state = 'decommissioned', offset = 5 WHERE vehicle = 'v3'",
    )
    .await;
    scenario_common::perf::within("coord143/onboarding-and-produce", t, Duration::from_secs(2));

    // b) The Kafka read side: latest == 6, the whole log is ONE batch of
    //    six records, the tombstone replays as key v3 / NULL value.
    assert_eq!(
        list_offsets_v5(&mut sock, corr, TOPIC, 0, -1).await,
        (0, -1, 6, -1),
        "ListOffsets latest = log end 6"
    );
    corr += 1;
    assert_eq!(
        list_offsets_v5(&mut sock, corr, TOPIC, 0, -2).await,
        (0, -1, 0, -1),
        "ListOffsets earliest = 0"
    );
    corr += 1;
    let (error, hwm, records) = fetch_v0(&mut sock, corr, TOPIC, 0).await;
    corr += 1;
    assert_eq!((error, hwm), (0, 6), "fetch from 0 clean, hwm = log end");
    let batch = parse_batch(&records).expect("CRC-verified record batch");
    assert_eq!(
        (batch.base_offset, batch.records.len() as i64),
        (0, 6),
        "one batch: base 0, six records"
    );
    assert_eq!(
        (batch.records[5].key.clone(), batch.records[5].value.clone()),
        (Some(b"v3".to_vec()), None),
        "record 5 is the tombstone: key v3, null value"
    );
    let latest = latest_by_key(&batch);
    let mut expected: Vec<(String, Option<String>, i64)> = VEHICLES
        .iter()
        .enumerate()
        .map(|(i, v)| (v.to_string(), Some(status_json(i)), i as i64))
        .collect();
    expected[3] = ("v3".to_string(), None, 5);
    assert_eq!(latest, expected, "latest record per vehicle, in offset order");
    assert_eq!(fetch_v0(&mut sock, corr, TOPIC, 6).await.0, 0, "EOF poll at the log end");

    // c) The RESP stream view: the same six entries, the tombstone as
    //    `k`=v3 + `__null__`="" with NO `v` field; no string-key view.
    let stream = format!("{TOPIC}/p0");
    assert_eq!(int_of(&r(&a, &[b"XLEN", stream.as_bytes()]).await), 6, "XLEN = log end");
    let xrange = rf(&a, &[b"XRANGE", stream.as_bytes(), b"-", b"+"]).await;
    assert_eq!(stream_ids(&xrange).len(), 6, "six entries by id: {xrange}");
    assert_eq!(
        xrange.matches("$1\r\nv\r\n").count(),
        5,
        "five live `v` fields, the tombstone carries none: {xrange}"
    );
    assert!(
        xrange.contains("$8\r\n__null__\r\n$0\r\n\r\n"),
        "tombstone marker pair `__null__`=\"\": {xrange}"
    );
    for (i, v) in VEHICLES.iter().enumerate() {
        assert!(xrange.contains(&status_json(i)), "status of {v} visible");
    }
    assert_eq!(
        rb(&a, &[b"GET", format!("{TOPIC}/v3").as_bytes()]).await,
        "",
        "kafka keys never materialize as string keys"
    );

    // d) SQL fleet table vs the fetched batch: alive set, latest offsets,
    //    the tombstone marks (not deletes) its row.
    let alive: Vec<MVal> = latest
        .iter()
        .filter(|(_, v, _)| v.is_some())
        .map(|(k, _, _)| s(k))
        .collect();
    assert_eq!(alive.len(), 4, "v3 is out after its tombstone");
    assert_eq!(
        col(
            &mut c,
            "SELECT vehicle FROM fleet WHERE state = 'alive' ORDER BY vehicle"
        )
        .await,
        alive,
        "SQL alive set == vehicles whose latest record is non-null"
    );
    let want_rows: Vec<Vec<MVal>> = latest
        .iter()
        .map(|(k, v, off)| {
            vec![
                s(k),
                s(if v.is_some() { "alive" } else { "decommissioned" }),
                common::mysql::int(*off),
            ]
        })
        .collect();
    assert_eq!(
        rows(&mut c, "SELECT vehicle, state, offset FROM fleet ORDER BY vehicle").await,
        want_rows,
        "table state/offset == the batch's latest records"
    );
    assert_eq!(
        rows(&mut c, "SELECT COUNT(*), MAX(offset) FROM fleet").await,
        vec![vec![common::mysql::int(5), common::mysql::int(5)]],
        "the tombstone keeps the row and owns the top offset"
    );
    scenario_common::perf::scenario("coord143", t0);
}
