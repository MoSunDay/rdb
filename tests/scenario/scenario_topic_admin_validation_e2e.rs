//! Multi-store scenario #156 -- an event platform's topic-admin validation ladder on
//! one broker: ApiVersions v1 bootstrap, then CreateTopics v4 / CreatePartitions v1 /
//! DescribeConfigs v1 walked from every rejection (17/37/38/39/36/42/3, the fixed
//! validation order, validate_only) to the real create (default 1 partition), the
//! grow to 3 and the 4-entry TOPIC defaults; Metadata v1 (broker/controller/leader/
//! replicas/isr rows) and Fetch v0 (unknown 3; an XADD on the bare topic lands in
//! `ledger156/pN` and fetches back as key/value). Every probe is a MySQL
//! `topic_admin_audit` row; its ok/err counts and code sums are recomputed Rust-side.
//! Kafka wire + RESP stream + MySQL. One REAL node, fronts kafka + resp + mysql.
//! Deviation: DescribeConfigs answers a bad name as 17 + "no such topic" and emits
//! the TOPIC defaults on EVERY row, error or BROKER-typed (admin_configs.rs:77-84);
//! the CreatePartitions unknown-topic message is NULL (admin_topics.rs:223-225).

use std::time::Duration;

use crate::common;
use crate::kafka_front_common;
use crate::scenario_common;
use common::mysql::{col, connect_root, ddl, ints, run, s};
use kafka_front_common::topics::{
    API_CREATE_PARTITIONS, API_CREATE_TOPICS, API_DESCRIBE_CONFIGS, API_METADATA, body_of,
};
use kafka_front_common::{kafka_req, kafka_port, kafka_round};
use rdb::kafka::frame::{Reader, put_array_len, put_i16, put_i32, put_i64, put_i8, put_string};
use rdb::kafka::record::parse_batch;
use scenario_common::{FrontSpec, bulk_at, front_world, int_of, r};
use tokio::net::TcpStream;

const TAG: &str = "kadmin156";
const TOPIC: &str = "ledger156";
const MISSING: &str = "nope156";
const BAD_NAME: &str = "bad name";
const NO_KEYS: &[&str] = &[];
const API_VERSIONS: i16 = 18;
const API_FETCH: i16 = 1;
// Kafka error codes pinned by this ladder (local consts).
const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
const INVALID_TOPIC_EXCEPTION: i16 = 17;
const TOPIC_ALREADY_EXISTS: i16 = 36;
const INVALID_PARTITIONS: i16 = 37;
const INVALID_REPLICATION_FACTOR: i16 = 38;
const INVALID_REPLICA_ASSIGNMENT: i16 = 39;
const INVALID_REQUEST: i16 = 42;
const RESOURCE_TYPE_TOPIC: i8 = 2;
const RESOURCE_TYPE_BROKER: i8 = 4;

/// Reply tail shared by CreateTopics v4 / CreatePartitions v1: throttle,
/// then one (name, error, nullable message) row -> (code, "" = null).
fn admin_row(payload: &[u8], name: &str) -> (i16, String) {
    let mut r = body_of(payload);
    assert_eq!(r.i32(), Some(0), "throttle_time_ms leads the reply");
    assert_eq!(r.array_len(), Some(Some(1)), "one topic row");
    assert_eq!(r.string().as_deref(), Some(name), "topic echoed");
    let code = r.i16().unwrap();
    (code, r.nullable_string().unwrap().unwrap_or_default())
}

/// CreateTopics v4 for one topic with explicit replica assignments
/// (`[broker_ids]` per partition) and the validate_only flag.
#[allow(clippy::too_many_arguments)]
async fn create_topics_raw(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    parts: i32,
    rf: i16,
    assignments: &[&[i32]],
    validate_only: bool,
) -> (i16, String) {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_i32(&mut b, parts);
    put_i16(&mut b, rf);
    put_array_len(&mut b, assignments.len());
    for (idx, brokers) in assignments.iter().enumerate() {
        put_i32(&mut b, idx as i32);
        put_array_len(&mut b, brokers.len());
        for id in *brokers {
            put_i32(&mut b, *id); // the single broker is node 1
        }
    }
    put_array_len(&mut b, 0); // configs: accepted and ignored
    put_i32(&mut b, 1_000); // timeout_ms
    b.push(validate_only as u8);
    let payload = kafka_round(sock, &kafka_req(API_CREATE_TOPICS, 4, corr, false, &b)).await;
    admin_row(&payload, topic)
}

/// CreatePartitions v1 to a new TOTAL count (`assignments` empty
/// broker_ids lists) with the validate_only flag.
async fn create_partitions_raw(sock: &mut TcpStream, corr: i32, topic: &str, count: i32, assignments: usize, validate_only: bool) -> (i16, String) {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_i32(&mut b, count);
    put_array_len(&mut b, assignments);
    for _ in 0..assignments {
        put_array_len(&mut b, 0); // one empty broker_ids list each
    }
    put_i32(&mut b, 1_000);
    b.push(validate_only as u8);
    let payload = kafka_round(sock, &kafka_req(API_CREATE_PARTITIONS, 1, corr, false, &b)).await;
    admin_row(&payload, topic)
}

/// DescribeConfigs v1 for one resource: (error, message, [(name, value)]).
async fn describe_configs(
    sock: &mut TcpStream,
    corr: i32,
    rtype: i8,
    name: &str,
    keys: &[&str],
) -> (i16, String, Vec<(String, String)>) {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_i8(&mut b, rtype);
    put_string(&mut b, name);
    put_array_len(&mut b, keys.len());
    for k in keys {
        put_string(&mut b, k);
    }
    b.push(1); // include_synonyms
    let payload = kafka_round(sock, &kafka_req(API_DESCRIBE_CONFIGS, 1, corr, false, &b)).await;
    let mut r = body_of(&payload);
    r.i32(); // throttle
    assert_eq!(r.array_len(), Some(Some(1)), "one resource row");
    let err = r.i16().unwrap();
    let msg = r.nullable_string().unwrap().unwrap_or_default();
    assert_eq!(r.i8(), Some(rtype), "resource type echoed");
    assert_eq!(r.string().as_deref(), Some(name), "resource name echoed");
    let mut out = Vec::new();
    for _ in 0..r.array_len().unwrap().unwrap_or(0) {
        let key = r.string().unwrap();
        let value = r.nullable_string().unwrap().unwrap_or_default();
        r.boolean(); // read_only
        r.i8(); // config_source
        r.boolean(); // is_sensitive
        r.array_len(); // synonyms
        out.push((key, value));
    }
    (err, msg, out)
}

/// One `[i32]` array of a Reader.
fn i32s(r: &mut Reader<'_>) -> Vec<i32> {
    (0..r.array_len().unwrap().unwrap_or(0)).map(|_| r.i32().unwrap()).collect()
}
/// Metadata v1 for `topics`: (brokers [(id, host, port)], controller_id,
/// [(error, name, [(error, idx, leader, replicas, isr)])]).
async fn metadata_v1(sock: &mut TcpStream, corr: i32, topics: &[&str]) -> (
    Vec<(i32, String, i32)>,
    i32,
    Vec<(i16, String, Vec<(i16, i32, i32, Vec<i32>, Vec<i32>)>)>,
) {
    let mut b = Vec::new();
    put_array_len(&mut b, topics.len());
    for t in topics {
        put_string(&mut b, t);
    }
    let payload = kafka_round(sock, &kafka_req(API_METADATA, 1, corr, false, &b)).await;
    let mut r = body_of(&payload);
    r.i32(); // throttle
    let mut brokers = Vec::new();
    for _ in 0..r.array_len().unwrap().unwrap_or(0) {
        brokers.push((r.i32().unwrap(), r.string().unwrap(), r.i32().unwrap()));
        r.nullable_string(); // rack
    }
    let controller = r.i32().unwrap();
    let mut out = Vec::new();
    for _ in 0..r.array_len().unwrap().unwrap_or(0) {
        let err = r.i16().unwrap();
        let name = r.string().unwrap();
        r.boolean(); // is_internal
        let mut parts = Vec::new();
        for _ in 0..r.array_len().unwrap().unwrap_or(0) {
            let perr = r.i16().unwrap();
            let idx = r.i32().unwrap();
            let leader = r.i32().unwrap();
            let reps = i32s(&mut r);
            let isr = i32s(&mut r);
            parts.push((perr, idx, leader, reps, isr));
        }
        out.push((err, name, parts));
    }
    assert_eq!(r.remaining(), 0, "v1 reply fully drained");
    (brokers, controller, out)
}

/// ApiVersions v1: (error, [(api_key, min, max)] in body order).
async fn api_versions_v1(sock: &mut TcpStream, corr: i32) -> (i16, Vec<(i16, i16, i16)>) {
    let payload = kafka_round(sock, &kafka_req(API_VERSIONS, 1, corr, false, &[])).await;
    let mut r = body_of(&payload);
    let err = r.i16().unwrap();
    let mut out = Vec::new();
    for _ in 0..r.array_len().unwrap().unwrap_or(0) {
        out.push((r.i16().unwrap(), r.i16().unwrap(), r.i16().unwrap()));
    }
    assert_eq!(r.i32(), Some(0), "throttle trails the array on v1");
    (err, out)
}

/// Fetch v0 (no wait) for one partition: (error, hwm, record-set bytes).
async fn fetch_v0(sock: &mut TcpStream, corr: i32, topic: &str, part: i32) -> (i16, i64, Vec<u8>) {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    put_i32(&mut b, 0); // max_wait_ms: no park
    put_i32(&mut b, 1); // min_bytes
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, part);
    put_i64(&mut b, 0); // fetch_offset
    put_i32(&mut b, 1 << 20);
    let payload = kafka_round(sock, &kafka_req(API_FETCH, 0, corr, false, &b)).await;
    let mut r = body_of(&payload);
    assert_eq!(r.array_len(), Some(Some(1)), "one topic");
    assert_eq!(r.string().unwrap(), topic);
    assert_eq!(r.array_len(), Some(Some(1)), "one partition");
    assert_eq!(r.i32(), Some(part));
    let err = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    let n = r.i32().unwrap_or(-1) as usize;
    let records = r.take(n).unwrap_or(&[]).to_vec();
    (err, hwm, records)
}

/// Full-drain reply text with a short quiet gap (array replies).
async fn drain(a: &str, args: &[&[u8]]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 20, 10_000).await)
}

/// The message a probed reply carried ("" = the null success message).
fn ladder_msg(rows: &[(&str, String)], probe: &str) -> String {
    rows.iter().find(|(p, _)| *p == probe).expect(probe).1.clone()
}

#[tokio::test]
async fn topic_admin_validation_ladder() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { kafka: true, mysql: true, ..Default::default() };
    let (front, a) = front_world(TAG, &spec).await;
    let mut c = connect_root(&front.node).await;
    let mut sock = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    // (probe, api, code): every reply becomes one audit row later;
    // (probe, message): the wording assertions.
    let mut audit: Vec<(&str, &str, i16)> = Vec::new();
    let mut msgs: Vec<(&str, String)> = Vec::new();
    let mut corr = 1;

    // a) Hot block: the bootstrap and the whole ladder.
    let t = scenario_common::perf::now();
    let (verr, apis) = api_versions_v1(&mut sock, corr).await;
    corr += 1;
    let assigned: [&[i32]; 1] = [&[1]]; // one replica on broker 1
    for (probe, topic, parts, rf, assign, want) in [
        ("ct/bad-name", BAD_NAME, 1, 1, 0usize, INVALID_TOPIC_EXCEPTION),
        ("ct/order-name-first", BAD_NAME, 0, 0, 1, INVALID_TOPIC_EXCEPTION),
        ("ct/assign", TOPIC, 1, 1, 1, INVALID_REPLICA_ASSIGNMENT),
        ("ct/order-assign-before-rf", TOPIC, 1, 2, 1, INVALID_REPLICA_ASSIGNMENT),
        ("ct/rf-0", TOPIC, 1, 0, 0, INVALID_REPLICATION_FACTOR),
        ("ct/rf-2", TOPIC, 1, 2, 0, INVALID_REPLICATION_FACTOR),
        ("ct/parts-0", TOPIC, 0, 1, 0, INVALID_PARTITIONS),
        ("ct/parts-10001", TOPIC, 10_001, 1, 0, INVALID_PARTITIONS),
        ("ct/validate-only", TOPIC, 3, 1, 0, 0),
    ] {
        let (code, msg) = create_topics_raw(&mut sock, corr, topic, parts, rf, &assigned[..assign], probe.ends_with("only")).await;
        corr += 1;
        assert_eq!(code, want, "{probe}: {msg}");
        audit.push((probe, "CreateTopics", code));
        msgs.push((probe, msg));
    }
    let (_, _, rows) = metadata_v1(&mut sock, corr, &[TOPIC]).await;
    corr += 1;
    assert_eq!(rows, vec![(UNKNOWN_TOPIC_OR_PARTITION, TOPIC.to_string(), vec![])], "validate_only created nothing: {rows:?}");
    for (probe, parts, rf, want) in [
        ("ct/default-1", -1, -1, 0),
        ("ct/duplicate", 1, 1, TOPIC_ALREADY_EXISTS),
    ] {
        let (code, msg) = create_topics_raw(&mut sock, corr, TOPIC, parts, rf, &[], false).await;
        corr += 1;
        assert_eq!(code, want, "{probe}: {msg}");
        audit.push((probe, "CreateTopics", code));
        msgs.push((probe, msg));
    }
    let (_, _, rows) = metadata_v1(&mut sock, corr, &[TOPIC]).await;
    corr += 1;
    assert_eq!(rows[0].2.iter().map(|p| p.1).collect::<Vec<_>>(), vec![0], "-1/-1 = one partition: {rows:?}");
    for (probe, topic, count, assign, want) in [
        ("cp/unknown", MISSING, 3, 0usize, UNKNOWN_TOPIC_OR_PARTITION),
        ("cp/count-0", TOPIC, 0, 0, INVALID_PARTITIONS),
        ("cp/count-neg", TOPIC, -1, 0, INVALID_PARTITIONS),
        ("cp/order-assign-before-count", TOPIC, 0, 1, INVALID_REPLICA_ASSIGNMENT),
        ("cp/grow-3", TOPIC, 3, 0, 0),
        ("cp/validate-only-5", TOPIC, 5, 0, 0),
        ("cp/shrink-2", TOPIC, 2, 0, INVALID_PARTITIONS),
        ("cp/same-3", TOPIC, 3, 0, 0),
        ("cp/assign", TOPIC, 5, 1, INVALID_REPLICA_ASSIGNMENT),
    ] {
        let (code, msg) = create_partitions_raw(&mut sock, corr, topic, count, assign, probe.ends_with("5")).await;
        corr += 1;
        assert_eq!(code, want, "{probe}: {msg}");
        audit.push((probe, "CreatePartitions", code));
        msgs.push((probe, msg));
    }
    let (brokers, controller, rows) = metadata_v1(&mut sock, corr, &[TOPIC, MISSING]).await;
    corr += 1;
    let grown = vec![0, 1, 2];
    for (probe, rtype, name, keys, want, entries) in [
        ("dc/broker-type", RESOURCE_TYPE_BROKER, "1", NO_KEYS, INVALID_REQUEST, 4usize),
        ("dc/unknown", RESOURCE_TYPE_TOPIC, MISSING, NO_KEYS, UNKNOWN_TOPIC_OR_PARTITION, 4),
        ("dc/bad-name", RESOURCE_TYPE_TOPIC, BAD_NAME, NO_KEYS, INVALID_TOPIC_EXCEPTION, 4),
        ("dc/unknown-key", RESOURCE_TYPE_TOPIC, TOPIC, &["nope.cfg"], 0, 0),
        ("dc/one-key", RESOURCE_TYPE_TOPIC, TOPIC, &["retention.ms"], 0, 1),
        ("dc/all", RESOURCE_TYPE_TOPIC, TOPIC, NO_KEYS, 0, 4),
    ] {
        let (code, msg, cfg) = describe_configs(&mut sock, corr, rtype, name, keys).await;
        corr += 1;
        assert_eq!((code, cfg.len()), (want, entries), "{probe}: {msg} {cfg:?}");
        audit.push((probe, "DescribeConfigs", code));
        msgs.push((probe, msg));
        if probe == "dc/all" {
            assert_eq!(cfg, vec![("cleanup.policy".to_string(), "delete".to_string()), ("retention.ms".to_string(), "604800000".to_string()), ("retention.bytes".to_string(), "-1".to_string()), ("min.insync.replicas".to_string(), "1".to_string())], "the TOPIC default set");
        }
    }
    let (ferr, fhwm, _) = fetch_v0(&mut sock, corr, MISSING, 0).await;
    corr += 1;
    audit.push(("fetch/unknown", "Fetch", ferr));
    // Two fields on the bare topic: positionally (key, value) on Fetch
    // (src/kafka/fetch_records.rs:158-165).
    let reply = drain(&a, &[b"XADD", TOPIC.as_bytes(), b"*", b"kind", b"meter-9", b"site", b"west-1"]).await;
    let stream = bulk_at(reply.as_bytes(), 0);
    assert!(reply.starts_with("*2\r\n") && stream.starts_with(&format!("{TOPIC}/p")), "{reply}");
    let part: i32 = stream.rsplit_once("/p").unwrap().1.parse().expect("partition index");
    let xlen = int_of(&r(&a, &[b"XLEN", stream.as_bytes()]).await);
    let (xerr, xhwm, records) = fetch_v0(&mut sock, corr, TOPIC, part).await;
    audit.push(("fetch/xadd", "Fetch", xerr));
    scenario_common::perf::within("kadmin156/validation-ladder", t, Duration::from_secs(2));

    // b) The ladder's wording, then the shapes.
    for (probe, want) in [
        ("ct/bad-name", "invalid topic name"),
        ("ct/parts-0", "partition count must only grow (1..=10000)"),
        ("ct/rf-0", "single-node broker: replication factor must be 1 or unset (-1)"),
        ("ct/assign", "replica assignments are not supported on a single-node broker"),
        ("ct/duplicate", "topic already exists"),
        ("ct/validate-only", ""), ("ct/default-1", ""), ("cp/unknown", ""), ("cp/grow-3", ""),
        ("cp/count-0", "partition count must only grow (1..=10000)"),
        ("cp/assign", "replica assignments are not supported on a single-node broker"),
        ("dc/broker-type", "only TOPIC resources are supported"),
        ("dc/unknown", "no such topic"), ("dc/bad-name", "no such topic"),
    ] {
        assert_eq!(ladder_msg(&msgs, probe), want, "{probe}");
    }
    assert_eq!(verr, 0, "ApiVersions v1 bootstrap");
    for key_range in [(API_CREATE_TOPICS, 0, 4), (API_CREATE_PARTITIONS, 0, 1), (API_DESCRIBE_CONFIGS, 0, 3), (API_METADATA, 0, 8)] {
        assert!(apis.contains(&key_range), "advertised {key_range:?}: {apis:?}");
    }
    assert_eq!((brokers.len(), brokers[0].0, brokers[0].2, controller), (1, 1, kafka_port(&front.kafka), 1), "the single broker is its own controller: {brokers:?}");
    assert_eq!((rows.len(), rows[0].0, rows[0].1.as_str()), (2, 0, TOPIC), "live topic row: {rows:?}");
    assert_eq!(rows[0].2.iter().map(|p| p.1).collect::<Vec<_>>(), grown, "3 partitions after the ladder: {rows:?}");
    for (perr, _, leader, reps, isr) in &rows[0].2 {
        assert_eq!((*perr, *leader, reps.clone(), isr.clone()), (0, 1, vec![1], vec![1]), "partition row");
    }
    assert_eq!((rows[1].0, rows[1].1.as_str(), rows[1].2.len()), (UNKNOWN_TOPIC_OR_PARTITION, MISSING, 0), "unknown topic row");
    assert_eq!((ferr, fhwm), (UNKNOWN_TOPIC_OR_PARTITION, -1), "Fetch never parks on an unknown topic");
    assert!(part < 3, "the XADD landed in a created partition: {stream}");
    assert_eq!((xlen, xerr, xhwm), (1, 0, 1), "stream depth == hwm");
    let batch = parse_batch(&records).expect("one batch");
    assert_eq!((batch.base_offset, batch.records.len()), (0, 1), "the XADD is ordinal 0 of {stream}");
    assert_eq!((batch.records[0].key.as_deref(), batch.records[0].value.as_deref()), (Some(b"meter-9".as_slice()), Some(b"west-1".as_slice())), "2-field XADD fetches back positionally");

    // c) The audit ledger: one row per probe; the ok/err counts, the
    //    CreateTopics code sum and the rejected-probe list are recomputed
    //    Rust-side from the same replies.
    ddl(&mut c, "CREATE TABLE topic_admin_audit (probe VARCHAR(48) NOT NULL, api VARCHAR(24) NOT NULL, code INT NOT NULL, outcome VARCHAR(8) NOT NULL, PRIMARY KEY (probe))").await;
    for (probe, api, code) in &audit {
        let outcome = if *code == 0 { "ok" } else { "err" };
        run(&mut c, &format!("INSERT INTO topic_admin_audit VALUES ('{probe}', '{api}', {code}, '{outcome}')")).await;
    }
    let ok_n = audit.iter().filter(|(_, _, code)| *code == 0).count() as i64;
    let err_n = audit.len() as i64 - ok_n;
    let ct_sum: i64 = audit.iter().filter(|(_, api, _)| *api == "CreateTopics").map(|(_, _, code)| i64::from(*code)).sum();
    let mut rejected: Vec<&str> = audit.iter().filter(|(_, _, code)| *code != 0).map(|(p, _, _)| *p).collect();
    rejected.sort_unstable();
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM topic_admin_audit WHERE outcome = 'ok'").await, vec![ok_n], "ok rows");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM topic_admin_audit WHERE outcome = 'err'").await, vec![err_n], "err rows");
    assert_eq!(ints(&mut c, "SELECT SUM(code) FROM topic_admin_audit WHERE api = 'CreateTopics'").await, vec![ct_sum], "CreateTopics code sum");
    assert_eq!(col(&mut c, "SELECT probe FROM topic_admin_audit WHERE outcome = 'err' ORDER BY probe").await, rejected.iter().map(|p| s(p)).collect::<Vec<_>>(), "rejected probes");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM topic_admin_audit WHERE api = 'CreatePartitions' AND outcome = 'ok'").await, vec![3], "grow / validate_only / same count");
    assert_eq!(audit.len(), 28, "every probe ledgered: {audit:?}");
    scenario_common::perf::scenario(TAG, t0);
}
