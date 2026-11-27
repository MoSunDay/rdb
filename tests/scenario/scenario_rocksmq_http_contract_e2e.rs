//! Multi-store scenario #165 -- a parcel-events HTTP bridge: six `<ms>-<seq>` events posted to RocksMQ
//! channel `parcel165` behind a bearer ladder (no/wrong bearer 401 before path classification, GET on a
//! known route 405 + `Allow: POST`, unknown path 404, malformed/over-range `delay_ms` 400); the SAME channel
//! is Kafka topic `parcel165` p0 and RESP stream `parcel165/q0` (XLEN == ListOffsets log end == 8; two
//! Kafka-produced records come back through `/consume`), which auto-creates `hub-a` at 0-0 under consumer
//! `http` (first three bodies base64, in order); `/pending` is the PEL summary, `/range` replays
//! `-`/`%2B`/`(id` bounds, `/ack` is idempotent and 404s an unknown group; the consumed bodies land in MySQL.
//! Surfaces: RocksMQ HTTP + RESP lite + Kafka + MySQL. One REAL node, fronts rocksmq + kafka + mysql.
//! Deviation: `/produce` replies TEXT (body = the id; api.rs:176); `/range` bounds are `begin`/`end`
//! (range.rs:35); `/ack` has no pending check (api.rs:184-216); OffsetFetch of the lite group is
//! `(0, -1)` (offsets_fetch.rs:161-185); last-delivered-id is the PERSISTED head (`0-0` after the
//! `/consume`, id3 once the first XACK lands it; offset.rs:143).

use crate::common;
use crate::common::mysql::{aff, col, connect_root, ddl, ints, s};
use crate::kafka_front_common;
use crate::scenario_common;
use rdb::kafka::record::BatchRecord;
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{front_world, int_of, r, FrontSpec};
use kafka_front_common::topics::{fetch_offset_v0_raw, list_offsets_v5};
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpStream;

const CH: &str = "parcel165";
const STREAM: &[u8] = b"parcel165/q0";
const GROUP: &str = "hub-a";
const HTTP_N: usize = 6;
const CONSUMED: usize = 3;
const BASE_MS: u64 = 1_700_000_000_000;
const KAFKA_BODIES: [&str; 2] = ["kafka-transfer-1", "kafka-transfer-2"];
const UNAUTHORIZED: &str = "unauthorized";
const NOT_FOUND: &str = "not found";
const METHOD_NOT_ALLOWED: &str = "method not allowed";
const BAD_QUERY: &str = "bad query string: ";
const NO_GROUP: &str = "no such consumer group";
const WRONG_BEARER: &str = "Bearer not-the-bridge-token";
/// `delay_ms` ceiling (365 days) and the first value past it (api.rs:36).
const MAX_DELAY_MS: u64 = 31_536_000_000;
const OVER_DELAY_MS: u64 = MAX_DELAY_MS + 1;

/// The six bridge payloads: deterministic `<ms>-<seq>` event names.
fn bodies() -> Vec<String> {
    (1..=HTTP_N).map(|i| format!("{}-{i}", BASE_MS + 7 * i as u64)).collect()
}

/// One bearer-authenticated RocksMQ round: (status, head, body).
async fn authed(mq: &str, method: &str, target: &str, body: &str) -> (u16, String, String) {
    let bearer = format!("Bearer {}", common::mq::TOKEN);
    common::mq::req(mq, method, target, body, &[("Authorization", &bearer)]).await
}

/// Case-insensitive `name: value` line match on a raw head block
/// (its last line carries no trailing CRLF).
fn has_header(head: &str, name: &str, value: &str) -> bool {
    let want = format!("{name}: {value}").to_ascii_lowercase();
    head.to_ascii_lowercase().lines().any(|l| l == want)
}

/// `<digits>-<digits>` stream id shape.
fn is_id(s: &str) -> bool {
    s.split_once('-').is_some_and(|(ms, seq)| {
        !ms.is_empty()
            && !seq.is_empty()
            && ms.bytes().chain(seq.bytes()).all(|b| b.is_ascii_digit())
    })
}

/// Full reply of one RESP command as text (quiet-gap reader, ~25-45ms).
async fn drain(a: &str, args: &[&[u8]]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 20, 10_000).await)
}

/// `{"msgs":[...]}` -> (id, decoded body) pairs; the object carries exactly `id` + `body`.
fn msgs(body: &str) -> Vec<(String, String)> {
    let v = scenario_common::json_of(body);
    assert_eq!(v.as_object().map(|o| o.len()), Some(1), "one top-level key: {body}");
    v["msgs"].as_array().expect("msgs array").iter().map(|m| {
        let o = m.as_object().unwrap_or_else(|| panic!("msg object: {m}"));
        assert_eq!(o.len(), 2, "id + body only: {m}");
        let b64 = o["body"].as_str().expect("body str");
        (o["id"].as_str().expect("id str").to_string(), scenario_common::codec::b64d(b64))
    }).collect()
}

/// Values of one flat RESP row as text: bulk payloads verbatim, `$-1` -> "nil",
/// `:N` -> "N"; `$N`/`*N` headers are consumed (nested arrays flatten).
fn values(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Some(n) = toks[i].strip_prefix('$') {
            if n == "-1" {
                out.push("nil".to_string());
            } else {
                i += 1;
                out.push(toks.get(i).copied().unwrap_or("").to_string());
            }
        } else if let Some(n) = toks[i].strip_prefix(':') {
            out.push(n.to_string());
        }
        i += 1;
    }
    out
}

/// The value that follows `name` in a flat (label, value) row.
fn label(row: &[String], name: &str) -> String {
    row.iter()
        .position(|t| t == name)
        .and_then(|at| row.get(at + 1))
        .cloned()
        .unwrap_or_else(|| panic!("no {name} in {row:?}"))
}

/// RESP text of one `[id, [f1, v1, ...]]` entry frame, lengths computed here.
fn entry(id: &str, fields: &[(&str, &str)]) -> String {
    let mut out = format!("*2\r\n${}\r\n{id}\r\n*{}\r\n", id.len(), fields.len() * 2);
    for (f, v) in fields {
        out.push_str(&format!("${}\r\n{f}\r\n${}\r\n{v}\r\n", f.len(), v.len()));
    }
    out
}

/// `[channel, group, pending, min, max, consumers]` of the `/pending` summary.
fn pel_json(pending: usize, min_id: &str, max_id: &str) -> serde_json::Value {
    json!({"channel": CH, "group": GROUP, "pending": pending, "min_id": min_id,
           "max_id": max_id, "consumers": [{"name": "http", "pending": pending}]})
}

#[tokio::test]
async fn rocksmq_http_contract() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        kafka: true,
        rocksmq: true,
        extra_yaml: format!("rocksmq_token: {}\n", common::mq::TOKEN),
        ..Default::default()
    };
    let (front, a) = front_world("rmq165", &spec).await;
    let mq = front.rocksmq.as_str();
    let bodies = bodies();
    let produce = format!("/produce?channel={CH}");

    // a) Route ladder: auth -> 404 "not found" -> 405 + Allow: POST -> 400 -> handler.
    let (st, _, body) = common::mq::req(mq, "POST", &produce, "x", &[]).await;
    assert_eq!((st, body.as_str()), (401, UNAUTHORIZED), "no Authorization header");
    let (st, _, body) = common::mq::req(mq, "POST", &produce, "x", &[("Authorization", WRONG_BEARER)]).await;
    assert_eq!((st, body.as_str()), (401, UNAUTHORIZED), "wrong bearer");
    let (st, _, body) = common::mq::req(mq, "GET", "/nope", "", &[]).await;
    assert_eq!((st, body.as_str()), (401, UNAUTHORIZED), "the bearer gate precedes path classification");
    let (st, head, body) = authed(mq, "GET", "/produce", "").await;
    assert_eq!((st, body.as_str()), (405, METHOD_NOT_ALLOWED), "GET on a known route");
    assert!(has_header(&head, "Allow", "POST"), "Allow: POST on the 405: {head}");
    let (st, _, body) = authed(mq, "GET", "/nope", "").await;
    assert_eq!((st, body.as_str()), (404, NOT_FOUND), "path is classified before the method");
    let (st, _, body) = authed(mq, "POST", "/nope", "").await;
    assert_eq!((st, body.as_str()), (404, NOT_FOUND), "unknown path");
    let ladder: [(&str, String); 3] = [
        ("/produce?channel=%ZZ", format!("{BAD_QUERY}malformed % escape")),
        (&format!("/produce?channel={CH}&delay_ms=abc"), "invalid 'delay_ms' parameter 'abc'".to_string()),
        (
            &format!("/produce?channel={CH}&delay_ms={OVER_DELAY_MS}"),
            format!("'delay_ms' must be 0..={MAX_DELAY_MS}, got {OVER_DELAY_MS}"),
        ),
    ];
    for (target, want) in ladder {
        let (st, _, got) = authed(mq, "POST", target, "x").await;
        assert_eq!((st, got.as_str()), (400, want.as_str()), "{target}");
    }

    // b) Hot block -- the bridge cycle: six produces, one group consume, the ack contract.
    let t = scenario_common::perf::now();
    let mut ids: Vec<String> = Vec::with_capacity(HTTP_N);
    for (i, b) in bodies.iter().enumerate() {
        let (st, head, body) = authed(mq, "POST", &produce, b).await;
        assert_eq!(st, 200, "produce {i}: {body}");
        assert!(has_header(&head, "Content-Type", "text/plain"), "text/plain: {head}");
        assert!(is_id(&body), "produce {i} replies the bare stream id: {body:?}");
        ids.push(body);
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids ascend in produce order: {ids:?}");
    let (st, _, body) = authed(mq, "POST", &format!("/consume?channel={CH}&group={GROUP}&n={CONSUMED}"), "").await;
    assert_eq!(st, 200, "consume: {body}");
    let got = msgs(&body);
    let first: Vec<(String, String)> = ids[..CONSUMED].iter().cloned().zip(bodies[..CONSUMED].iter().cloned()).collect();
    assert_eq!(got, first, "the first three bodies, base64, in id order");
    let groups = drain(&a, &[b"XINFO", b"GROUPS", STREAM]).await;
    let row = values(&groups);
    assert!(groups.starts_with("*1\r\n*13\r\n") && row.len() == 13, "one 13-element row: {groups}");
    // last-delivered-id is the PERSISTED head: the consume moves it in memory only (offset.rs:143).
    assert_eq!(
        (row[0].as_str(), label(&row, "last-delivered-id").as_str(), label(&row, "committed-id").as_str()),
        (GROUP, "0-0", "0-0"),
        "hub-a was created at 0-0; nothing has landed the delivery head yet: {groups}"
    );
    let consumers = values(&drain(&a, &[b"XINFO", b"CONSUMERS", STREAM, GROUP.as_bytes()]).await);
    assert_eq!(
        (label(&consumers, "name").as_str(), label(&consumers, "pending").as_str()),
        ("http", "3"),
        "the HTTP front is ONE consumer named http: {consumers:?}"
    );
    let (st, head, body) = authed(mq, "POST", &format!("/pending?channel={CH}&group={GROUP}"), "").await;
    assert_eq!(st, 200, "pending: {body}");
    assert!(has_header(&head, "Content-Type", "application/json"), "JSON: {head}");
    assert_eq!(scenario_common::json_of(&body), pel_json(3, &ids[0], &ids[2]), "PEL summary after the consume");
    let ack = format!("/ack?channel={CH}&group={GROUP}&id=");
    for (id, what) in [(ids[0].as_str(), "the first pending id"), ("1-1", "a well-formed NON-pending id")] {
        let (st, _, body) = authed(mq, "POST", &format!("{ack}{id}"), "").await;
        assert_eq!((st, body.as_str()), (200, "ok"), "ack of {what}");
    }
    let (st, _, body) = authed(mq, "POST", &format!("/ack?channel={CH}&group=no-such-group&id={}", ids[0]), "").await;
    assert_eq!((st, body.as_str()), (404, NO_GROUP), "ack with an unknown group");
    let (st, _, body) = authed(mq, "POST", &format!("/pending?channel={CH}&group={GROUP}"), "").await;
    assert_eq!((st, scenario_common::json_of(&body)), (200, pel_json(2, &ids[1], &ids[2])), "after the acks");
    let row = values(&drain(&a, &[b"XINFO", b"GROUPS", STREAM]).await);
    assert_eq!(label(&row, "committed-id"), ids[0], "the ack lands the committed watermark on id1");
    scenario_common::perf::within("rmqhttp/bridge-cycle", t, Duration::from_secs(2));

    // c) /range replay: `-`..`%2B` (a raw `+` decodes to a space), `(id` exclusive, id..id.
    let (st, _, body) = authed(mq, "POST", &format!("/range?channel={CH}&begin=-&end=%2B"), "").await;
    assert_eq!(st, 200, "range all: {body}");
    let all: Vec<(String, String)> = ids.iter().cloned().zip(bodies.iter().cloned()).collect();
    assert_eq!(msgs(&body), all, "ascending id order over the whole channel");
    let (st, _, body) = authed(mq, "POST", &format!("/range?channel={CH}&begin=({}&end=%2B", ids[0]), "").await;
    assert_eq!((st, msgs(&body)), (200, all[1..].to_vec()), "an exclusive (id start skips id1");
    let (st, _, body) = authed(mq, "POST", &format!("/range?channel={CH}&begin={}&end={}", ids[1], ids[2]), "").await;
    assert_eq!((st, msgs(&body)), (200, all[1..CONSUMED].to_vec()), "an inclusive id..id window");

    // d) Cross-protocol identity: the channel is the Kafka topic p0 and the RESP stream.
    assert_eq!(int_of(&r(&a, &[b"XLEN", STREAM]).await), HTTP_N as i64, "six HTTP events on parcel165/q0");
    let xr = drain(&a, &[b"XRANGE", STREAM, b"-", b"+"]).await;
    let want_xr: String = ids.iter().zip(&bodies).map(|(id, b)| entry(id, &[("v", b)])).collect();
    assert_eq!(xr, format!("*{HTTP_N}\r\n{want_xr}"), "the RESP face reads the HTTP events as (v, body) entries");
    let mut sock = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    let (mut corr, latest): (i32, i64) = (1, -1);
    assert_eq!(list_offsets_v5(&mut sock, corr, CH, 0, latest).await, (0, -1, HTTP_N as i64, -1), "log end == XLEN");
    corr += 1;
    assert_eq!(list_offsets_v5(&mut sock, corr, CH, 0, -2).await, (0, -1, 0, -1), "earliest ordinal 0");
    corr += 1;
    assert_eq!(fetch_offset_v0_raw(&mut sock, corr, GROUP, CH, 0).await, (0, -1), "the lite group holds NO Kafka ledger row");
    corr += 1;
    for (i, body) in KAFKA_BODIES.iter().enumerate() {
        let record = BatchRecord { timestamp_delta: 0, key: None, value: Some(body.as_bytes()), headers: vec![] };
        let base = kafka_produce_v3(&front.kafka, corr + i as i32, CH, &[record]).await;
        assert_eq!(base as i64, (HTTP_N + i) as i64, "Kafka produce appends after the HTTP events");
    }
    corr += KAFKA_BODIES.len() as i32;
    assert_eq!(int_of(&r(&a, &[b"XLEN", STREAM]).await), (HTTP_N + KAFKA_BODIES.len()) as i64, "RESP sees the Kafka records");
    assert_eq!(list_offsets_v5(&mut sock, corr, CH, 0, latest).await, (0, -1, (HTTP_N + KAFKA_BODIES.len()) as i64, -1), "Kafka log end 8");
    let (st, _, body) = authed(mq, "POST", &format!("/consume?channel={CH}&group={GROUP}&n=5"), "").await;
    assert_eq!(st, 200, "second consume: {body}");
    let tail = msgs(&body);
    let mut want_tail: Vec<(String, String)> = all[CONSUMED..].to_vec();
    want_tail.extend(tail[HTTP_N - CONSUMED..].iter().map(|(id, _)| id.clone()).zip(KAFKA_BODIES.iter().map(|s| s.to_string())));
    assert_eq!(tail.len(), HTTP_N - CONSUMED + KAFKA_BODIES.len(), "the rest of the channel in one pull");
    assert_eq!(tail, want_tail, "events 4-6 then the two Kafka records, base64, in order");
    let (st, _, body) = authed(mq, "POST", &format!("/pending?channel={CH}&group={GROUP}"), "").await;
    let last = tail.last().map(|(id, _)| id.clone()).unwrap_or_default();
    assert_eq!((st, scenario_common::json_of(&body)), (200, pel_json(HTTP_N + KAFKA_BODIES.len() - 1, &ids[1], &last)), "pending = 8 entries - 1 acked");
    let xr = drain(&a, &[b"XRANGE", STREAM, b"-", b"+"]).await;
    let kafka_frames: String = tail[HTTP_N - CONSUMED..].iter().zip(KAFKA_BODIES).map(|((id, _), b)| entry(id, &[("v", b)])).collect();
    assert_eq!(xr, format!("*{}\r\n{want_xr}{kafka_frames}", HTTP_N + KAFKA_BODIES.len()), "XRANGE: eight entries, HTTP events then the Kafka records as (v, body)");

    // e) The audit table carries the three CONSUMED (decoded) bodies.
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE parcel_audit (seq INT PRIMARY KEY, body VARCHAR(128))").await;
    let cells: Vec<String> = got.iter().enumerate().map(|(i, (_, b))| format!("({}, '{}')", i + 1, b)).collect();
    assert_eq!(aff(&mut c, &format!("INSERT INTO parcel_audit VALUES {}", cells.join(", "))).await, CONSUMED as u64, "three audit rows");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM parcel_audit").await, vec![CONSUMED as i64], "audit count");
    assert_eq!(col(&mut c, "SELECT body FROM parcel_audit WHERE seq = 2").await, vec![s(&got[1].1)], "seq 2 = the second consumed body");
    assert_eq!(ints(&mut c, &format!("SELECT seq FROM parcel_audit WHERE body <> '{}' ORDER BY seq", got[0].1)).await, vec![2, 3], "a filter over the audited bodies");
    scenario_common::perf::scenario("rmqhttp", t0);
}
