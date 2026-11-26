//! Multi-store scenario #160 -- a platform operator's metrics census on one real store: every registered
//! metrics family is in the `/metrics` exposition with its TYPE/HELP, and the shift's traffic lands in the
//! right series -- 12 INCRs -> `rdb_command_latency`, a MULTI/EXEC batch -> `rdb_tx_events` +
//! `rdb_tx_commit_latency`, a SQL table + BEGIN/INSERT/COMMIT -> `rdb_sql_query_latency` kinds, one XADD +
//! two Kafka produces -> `rdb_lite_messages{op="add"}`, `rdb_lite_streams{kind="live"}` and `rdb_kafka_api_latency`
//! (Produce/Fetch), the RESP and Kafka planes reading the same records. The raft control plane answers RAFT
//! SET/GET, `/get`, 401 for a wrong `/join` token, and a join to an unreachable peer holds ONLY the mux for 30s.
//! SURFACES: monitor HTTP + raft control HTTP + RESP + Kafka + MySQL. One REAL node, fronts kafka + mysql.
//! Deviation: `rdb_sql_tx_active`/`rdb_sql_tx_total` never move (no caller of sql_tx_enter/exit,
//! src/monitor.rs:209-219); counters have no `_total` suffix (prometheus 0.13); three families appear
//! only after first use (`rdb_lite_messages`, `rdb_sql_query_latency`, `rdb_kafka_api_latency`); the
//! timed-out join leaves a joint config (dead peer listed as voter) effective-but-uncommitted.

use crate::common;
use crate::kafka_front_common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use common::TOKEN;
use kafka_front_common::{kafka_req, kafka_round};
use rdb::kafka::frame::{Reader, put_array_len, put_i32, put_i64, put_string};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{bulk_at, front_world, int_of, ok, r, rb, rt, FrontSpec};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TAG: &str = "census160";
const TOPIC: &str = "census160";
/// RESP-only stream (auto-picks `q0`), kept apart from the Kafka topic.
const STREAM: &str = "census160resp";
/// RESP keys share one hash tag so a MULTI/EXEC batch passes the slot check.
const COUNTER: &str = "{census160}:counter";
const TXKEY: &str = "{census160}:tx";
const KEY: &str = "census160:raft";
const VALUE: &str = "v160";
const INCRS: i64 = 12;
const API_FETCH: i16 = 1;
const WRONG: &str = "not-the-raft-token";
/// INCR / GET series as the exposition spells them (label names sorted).
const INCR_HIST: &str = "rdb_command_latency_count{ack=\"false\",mode=\"incr\",type=\"normal\"}";
const GET_HIST: &str = "rdb_command_latency_count{ack=\"false\",mode=\"get\",type=\"normal\"}";
const QUEUED: &str = "rdb_tx_events{event=\"queued\"}";
const COMMITS: &str = "rdb_tx_events{event=\"commits\"}";
/// `COMMAND DOCS get` / `COMMAND INFO get frobnicate` / `INFO nosuchsection`, wire-spelled.
const DOCS_GET: &str = "*2\r\n$3\r\nget\r\n*4\r\n$7\r\nsummary\r\n$3\r\nget\r\n$5\r\nsince\r\n$5\r\n1.0.0\r\n";
const INFO_GET: &str = "*2\r\n*6\r\n$3\r\nget\r\n:2\r\n*0\r\n:1\r\n:1\r\n:1\r\n$-1\r\n";
/// `(family, TYPE, HELP)` of every registered collector (src/monitor.rs:47-123).
const FAMILIES: [(&str, &str, &str); 13] = [
    ("rdb_command_latency", "histogram", "rdb command latency(millisecond)"),
    ("raft_stats", "gauge", "raft stats"),
    ("rdb_lite_messages", "counter", "lite mode message operations"),
    ("rdb_lite_streams", "gauge", "lite mode stream counts"),
    ("rdb_lite_offset_dirty", "gauge", "lite group offsets pending flush"),
    ("rdb_lite_backlog", "gauge", "lite unacked pending stream entries (PEL) across consumer groups"),
    ("rdb_lite_dlq_depth", "gauge", "lite dead-letter stream entry depth (summed)"),
    ("rdb_tx_events", "counter", "redis multi/exec transaction events"),
    ("rdb_tx_commit_latency", "histogram", "multi/exec replay latency (millisecond)"),
    ("rdb_sql_query_latency", "histogram", "sql statement latency(millisecond)"),
    ("rdb_sql_tx_active", "gauge", "sql transactions in flight"),
    ("rdb_sql_tx_total", "counter", "sql transaction outcomes"),
    ("rdb_kafka_api_latency", "histogram", "kafka front api latency(millisecond)"),
];
/// The only families with zero-initialized children: `rdb_lite_messages`,
/// `rdb_sql_query_latency` and `rdb_kafka_api_latency` stay out of the
/// exposition until their first observation (prometheus drops empty families).
const BOOT: [&str; 10] = ["raft_stats", "rdb_command_latency", "rdb_lite_backlog", "rdb_lite_dlq_depth", "rdb_lite_offset_dirty", "rdb_lite_streams", "rdb_sql_tx_active", "rdb_sql_tx_total", "rdb_tx_commit_latency", "rdb_tx_events"];

/// One raw HTTP GET round trip (`Connection: close`) with its own client
/// deadline: (status, body). The raft control plane parks a membership
/// change for 30s -- longer than `scenario_common::http` is willing to
/// wait, hence the explicit `wait`.
async fn http_raw(addr: &str, target: &str, wait: Duration) -> (u16, String) {
    let req = format!("GET {target} HTTP/1.1\r\nHost: census\r\nConnection: close\r\n\r\n");
    let mut sock = match TcpStream::connect(addr).await { Ok(s) => s, Err(_) => return (0, String::new()) };
    sock.write_all(req.as_bytes()).await.expect("write request");
    let mut buf = Vec::new();
    let read = tokio::time::timeout(wait, sock.read_to_end(&mut buf)).await;
    let Ok(Ok(_)) = read else {
        return (0, String::new());
    };
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text.split(' ').nth(1).and_then(|t| t.parse().ok()).unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string();
    (status, body)
}

/// Value of one exposition sample line (`name{labels} value`); `None`
/// when the series is absent (a labeled family emits nothing before its
/// first child is born -- prometheus client semantics).
fn sample(body: &str, series: &str) -> Option<f64> {
    body.lines()
        .find(|l| l.starts_with(&format!("{series} ")))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
}

/// Movement of one series across two scrapes (an absent series counts 0).
fn moved(before: &str, after: &str, series: &str) -> f64 {
    sample(after, series).unwrap_or_default() - sample(before, series).unwrap_or_default()
}

/// One `/metrics` scrape: the exposition body.
async fn scrape(monitor: &str) -> String {
    let (status, body) = http_raw(monitor, "/metrics", Duration::from_secs(10)).await;
    assert_eq!(status, 200, "/metrics: {body}");
    body
}

/// Scrape until the 5s raft ticker (src/main.rs:178-186) published
/// `Leader`; returns that exposition body.
async fn metrics_leader(monitor: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let body = scrape(monitor).await;
        if sample(&body, "raft_stats{status=\"Leader\"}") == Some(1.0) {
            return body;
        }
        assert!(Instant::now() < deadline, "raft gauge never reached Leader: {body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The census itself: every family of `present` carries its TYPE and
/// HELP lines, every other registered family is absent (prometheus
/// omits a family without children), and nothing unlisted is registered.
fn census(body: &str, present: &[&str]) {
    for (name, kind, help) in FAMILIES {
        if present.contains(&name) {
            assert!(body.contains(&format!("# TYPE {name} {kind}\n")), "{name}: {body}");
            assert!(body.contains(&format!("# HELP {name} {help}\n")), "{name}: {body}");
        } else {
            assert!(!body.contains(&format!("# TYPE {name} ")), "{name} before first use: {body}");
        }
    }
    let typed: Vec<&str> = body.lines().filter_map(|l| l.strip_prefix("# TYPE ")).collect();
    assert_eq!(typed.len(), present.len(), "unlisted families: {typed:?}");
    for line in typed {
        assert!(FAMILIES.iter().any(|(n, k, _)| line == format!("{n} {k}")), "{line}");
    }
}

/// Fetch v0 round for (topic, partition 0, offset): (error, hwm,
/// record-set bytes); shape of scenario_kafka_offset_epoch_e2e.
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
    assert_eq!((r.array_len(), r.string()), (Some(Some(1)), Some(topic.to_string())), "one topic echoed");
    assert_eq!((r.array_len(), r.i32()), (Some(Some(1)), Some(0)), "one partition 0");
    let error = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    let n = r.i32().unwrap_or(-1) as usize;
    let records = r.take(n).unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0, "v0 reply fully drained");
    (error, hwm, records)
}

/// One Produce v3 record.
fn record<'a>(key: &'a [u8], value: &'a [u8]) -> rdb::kafka::record::BatchRecord<'a> {
    rdb::kafka::record::BatchRecord {
        timestamp_delta: 0,
        key: Some(key),
        value: Some(value),
        headers: vec![],
    }
}

/// One round trip on a fresh AUTH'd session (MULTI/EXEC state is per
/// connection): AUTH and every command of `cmds` are written back to
/// back, the replies drain until a 60ms quiet gap (the first byte may
/// lag 3s on a loaded box; EXEC's nested array arrives in pieces).
async fn session(addr: &str, cmds: &[&[&[u8]]]) -> Vec<u8> {
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    let auth = common::lite::frame(&[b"AUTH", TOKEN.as_bytes()]);
    sock.write_all(&auth).await.expect("write auth");
    for args in cmds {
        sock.write_all(&common::lite::frame(args)).await.expect("write");
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let wait = if buf.is_empty() {
            deadline.saturating_duration_since(Instant::now())
        } else {
            Duration::from_millis(60)
        };
        if wait.is_zero() {
            break;
        }
        match tokio::time::timeout(wait, sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    buf
}

/// Full-drain text of a RESP array reply (one-shot connection).
async fn full(a: &str, args: &[&str]) -> String {
    let argv: Vec<&[u8]> = args.iter().map(|t| t.as_bytes()).collect();
    common::lite::text(&common::lite::cmd_full_reply_wait(a, TOKEN, &argv, 20, 10_000).await)
}

/// `/join` or `/depart` for `peer` with the right token on the 45s
/// client path, off the test task: (status, body) once the server answers.
fn spawn_membership(http: &str, route: &str, peer: &str) -> tokio::task::JoinHandle<(u16, String)> {
    let target = format!("/{route}?peerAddress={peer}&raft-token={TOKEN}");
    let http = http.to_string();
    tokio::spawn(async move {
        let (status, body) = http_raw(&http, &target, Duration::from_secs(45)).await;
        (status, body)
    })
}

#[tokio::test]
async fn metrics_census_and_raft_kv() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        mysql: true,
        extra_yaml: "kafka_auto_create_topics: true\n".to_string(),
        ..Default::default()
    };
    let (front, a) = front_world(TAG, &spec).await;
    let node = &front.node;
    common::wait_leader(std::slice::from_ref(node), 60).await;
    let (http, monitor, raft) = (node.http.clone(), node.monitor.clone(), node.raft.clone());

    // a) The census on the first Leader scrape: the ten zero-initialized
    //    families with their TYPE/HELP, the three not-yet-used families
    //    absent, nothing unlisted, one Leader, zero children from boot.
    let m0 = metrics_leader(&monitor).await;
    // Families sorted by name (prometheus `gather`), HELP before TYPE.
    assert!(m0.starts_with("# HELP raft_stats raft stats\n# TYPE raft_stats gauge\n"), "text exposition: {}", &m0[..m0.len().min(200)]);
    census(&m0, &BOOT);
    for state in ["Shutdown", "Follower", "Candidate", "Unknown"] { assert_eq!(sample(&m0, &format!("raft_stats{{status=\"{state}\"}}")), Some(0.0), "{m0}"); }
    for series in ["rdb_lite_streams{kind=\"reaped\"}", "rdb_lite_offset_dirty", "rdb_lite_backlog", "rdb_lite_dlq_depth", "rdb_sql_tx_active", "rdb_sql_tx_total{outcome=\"commit\"}", "rdb_sql_tx_total{outcome=\"abort\"}", "rdb_tx_commit_latency_count", "rdb_tx_events{event=\"aborts\"}", "rdb_tx_events{event=\"conflicts\"}"] { assert_eq!(sample(&m0, series), Some(0.0), "{series} at boot: {m0}"); }

    // b) Hot block: 12 INCRs + GET, bracketed by scrapes -- exactly one
    //    histogram sample per command, the GET reads back the same 12.
    let t = scenario_common::perf::now();
    for _ in 0..INCRS {
        r(&a, &[b"INCR", COUNTER.as_bytes()]).await;
    }
    let got = r(&a, &[b"GET", COUNTER.as_bytes()]).await;
    let m1 = scrape(&monitor).await;
    scenario_common::perf::within("census160/12-incr+get+census-read", t, Duration::from_secs(2));
    assert_eq!(bulk_at(&got, 0), INCRS.to_string(), "GET after 12 INCRs: {got:?}");
    assert_eq!(moved(&m0, &m1, INCR_HIST), 12.0, "{m1}");
    assert_eq!(moved(&m0, &m1, GET_HIST), 1.0, "{m1}");

    // c) MULTI/EXEC on one session: two QUEUED replies, one EXEC -> queued
    //    +2, commits +1, one replay latency sample, and the replayed INCR
    //    lands in the very same command histogram.
    let tx: &[&[&[u8]]] = &[&[b"MULTI"], &[b"SET", TXKEY.as_bytes(), b"batch"], &[b"INCR", COUNTER.as_bytes()], &[b"EXEC"]];
    let wire = session(&a, tx).await;
    assert_eq!(wire, b"+OK\r\n+OK\r\n+QUEUED\r\n+QUEUED\r\n*2\r\n+OK\r\n:13\r\n", "ordered replay");
    let m2 = scrape(&monitor).await;
    assert_eq!(moved(&m1, &m2, QUEUED), 2.0, "{m2}");
    assert_eq!(moved(&m1, &m2, COMMITS), 1.0, "{m2}");
    assert_eq!(moved(&m1, &m2, "rdb_tx_commit_latency_count"), 1.0, "{m2}");
    assert_eq!(moved(&m1, &m2, INCR_HIST), 1.0, "replayed INCR observed: {m2}");
    assert_eq!(moved(&m1, &m2, "rdb_tx_events{event=\"aborts\"}"), 0.0, "{m2}");
    assert_eq!(rb(&a, &[b"GET", TXKEY.as_bytes()]).await, "batch", "EXEC applied the SET");

    // d) SQL: one statement per kind (ddl / insert / select) of a committed
    //    unit of work; the SQL transaction counters stay put (deviation).
    let mut c = connect_root(node).await;
    let m3 = scrape(&monitor).await;
    ddl(&mut c, "CREATE TABLE census160_notes(id INT PRIMARY KEY, note VARCHAR(32))").await;
    run(&mut c, "BEGIN").await;
    run(&mut c, "INSERT INTO census160_notes (id, note) VALUES (1, 'shipped'), (2, 'returned')").await;
    run(&mut c, "COMMIT").await;
    let m4 = scrape(&monitor).await;
    assert_eq!(moved(&m3, &m4, "rdb_sql_query_latency_count{kind=\"ddl\"}"), 1.0, "{m4}");
    assert_eq!(moved(&m3, &m4, "rdb_sql_query_latency_count{kind=\"insert\"}"), 1.0, "{m4}");
    assert_eq!(ints(&mut c, "SELECT id FROM census160_notes ORDER BY id").await, vec![1, 2], "committed");
    let m5 = scrape(&monitor).await;
    assert_eq!(moved(&m4, &m5, "rdb_sql_query_latency_count{kind=\"select\"}"), 1.0, "{m5}");
    for series in ["rdb_sql_tx_active", "rdb_sql_tx_total{outcome=\"commit\"}", "rdb_sql_tx_total{outcome=\"abort\"}"] { assert_eq!(sample(&m5, series), Some(0.0), "sql tx counters never move: {series} in {m5}"); }

    // e) Hot block: one RESP XADD + two Kafka produces (auto-created topic)
    //    -> `add` +3, Produce +2; the 200ms loop then counts two live
    //    streams and both planes hold the same records.
    assert!(r(&a, &[b"XADD", STREAM.as_bytes(), b"*", b"evt", b"resp"]).await.starts_with(b"*2"), "xadd");
    let t = scenario_common::perf::now();
    assert_eq!(kafka_produce_v3(&front.kafka, 1, TOPIC, &[record(b"k1", b"v1")]).await, 0, "first batch");
    assert_eq!(kafka_produce_v3(&front.kafka, 2, TOPIC, &[record(b"k2", b"v2")]).await, 1, "second batch");
    let m6 = scrape(&monitor).await;
    scenario_common::perf::within("census160/2-produces+census-read", t, Duration::from_secs(2));
    assert_eq!(moved(&m5, &m6, "rdb_lite_messages{op=\"add\"}"), 3.0, "{m6}");
    assert_eq!(moved(&m5, &m6, "rdb_kafka_api_latency_count{api=\"Produce\"}"), 2.0, "{m6}");
    let live = "rdb_lite_streams{kind=\"live\"}";
    let want = sample(&m5, live).unwrap_or_default() + 2.0;
    let deadline = Instant::now() + Duration::from_secs(5);
    let m7 = loop {
        let m = scrape(&monitor).await;
        if sample(&m, live) == Some(want) { break m; }
        assert!(Instant::now() < deadline, "live streams never reached {want}: {m}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(moved(&m5, &m7, live), 2.0, "one stream per plane: {m7}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"census160/p0"]).await), 2, "kafka topic == stream census160/p0");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"census160resp/q0"]).await), 1, "the XADD pinned q0");

    // f) The Kafka plane reads both records back in one batch at hwm 2;
    //    every histogram now shows its bucket/sum/count children.
    let mut sock = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    let (err, hwm, records) = fetch_v0(&mut sock, 7, TOPIC, 0).await;
    let m8 = scrape(&monitor).await;
    assert_eq!((err, hwm), (0, 2), "fetch from 0");
    let batch = rdb::kafka::record::parse_batch(&records).expect("batch parses");
    assert_eq!((batch.base_offset, batch.records.len() as i64), (0, 2), "one batch, two records");
    let txt = |b: Option<&[u8]>| String::from_utf8_lossy(b.unwrap_or_default()).into_owned();
    let pairs: Vec<(String, String)> = batch.records.iter().map(|rec| (txt(rec.key.as_deref()), txt(rec.value.as_deref()))).collect();
    assert_eq!(pairs, [("k1".to_string(), "v1".to_string()), ("k2".to_string(), "v2".to_string())]);
    assert_eq!(moved(&m6, &m8, "rdb_kafka_api_latency_count{api=\"Fetch\"}"), 1.0, "{m8}");
    census(&m8, &FAMILIES.map(|f| f.0)); // every family has children now
    for (name, kind, _) in FAMILIES { if kind == "histogram" { for suffix in ["_bucket", "_sum", "_count"] { assert!(m8.contains(&format!("{name}{suffix}")), "{name}{suffix}: {m8}"); } } }

    // g) Raft control plane: RAFT SET/GET on RESP, `/get` on HTTP
    //    (lock-free), the wrong-token 401, `raft nodes` = one voter.
    ok(&r(&a, &[b"RAFT", b"SET", KEY.as_bytes(), VALUE.as_bytes()]).await, "raft set");
    assert_eq!(rb(&a, &[b"RAFT", b"GET", KEY.as_bytes()]).await, VALUE, "raft get");
    assert_eq!(rb(&a, &[b"RAFT", b"GET", b"census160:absent"]).await, "", "missing key -> nil bulk");
    assert_eq!(rt(&a, &[b"RAFT", b"SET", b"census160:only"]).await, "-ERR wrong number of arguments for set command");
    assert_eq!(rt(&a, &[b"RAFT", b"GET"]).await, "-ERR wrong number of arguments for get command");
    assert_eq!(rt(&a, &[b"RAFT", b"frobnicate"]).await, "+raft [ help | stats | nodes | leader ]");
    let five = Duration::from_secs(5);
    assert_eq!(http_raw(&http, &format!("/get?key={KEY}&raft-token={TOKEN}"), five).await, (200, "v160\n".to_string()), "/get reads the live FSM");
    for query in [format!("/get?key=census160:absent&raft-token={TOKEN}"), format!("/get?key={KEY}&raft-token={WRONG}")] {
        assert_eq!(http_raw(&http, &query, five).await, (200, "\n".to_string()), "{query}: empty value");
    }
    assert_eq!(http_raw(&http, &format!("/join?peerAddress=127.0.0.1:1&raft-token={WRONG}"), five).await, (401, "unauthorized\n".to_string()), "wrong token on /join");
    assert_eq!(http_raw(&http, &format!("/join?raft-token={WRONG}"), five).await, (200, "invalid peerAddress\n".to_string()), "peerAddress checked first");
    let nodes_line = format!("+{raft} [Leader], nodes: [{{Suffrage:Voter ID:{raft} Address:{raft}}}]");
    assert_eq!(rt(&a, &[b"RAFT", b"NODES"]).await, nodes_line, "one voter, Leader");

    // h) Server introspection agrees with the command table and the binds.
    assert_eq!(full(&a, &["COMMAND", "DOCS", "get"]).await, DOCS_GET, "summary == name, since 1.0.0");
    assert_eq!(full(&a, &["COMMAND", "INFO", "get", "frobnicate"]).await, INFO_GET, "nil for unknown");
    let port = a.rsplit(':').next().unwrap_or("");
    let info = full(&a, &["INFO", "server"]).await;
    assert!(info.contains("# Server\r\nredis_version:7.4.0\r\nrdb_implementation:RocksDB\r\n"), "{info}");
    for field in [format!("tcp_port:{port}\r\n"), "process_id:".to_string(), "run_id:".to_string(), "uptime_in_seconds:".to_string()] {
        assert!(info.contains(&field) && !info.contains("# Cluster"), "{field:?} in {info}");
    }
    assert_eq!(full(&a, &["INFO", "nosuchsection"]).await, "$0\r\n\r\n", "unknown section -> empty bulk");

    // i) A join to an unreachable peer takes the membership mux for its
    //    whole 30s timeout (rcache/http.rs:65). Everything else answers
    //    meanwhile -- RESP, `/get` (lock-free), the 401 (checked before
    //    the mux), the scrape -- and a second membership call queues
    //    behind the mux until the join gives up.
    let peer = "127.0.0.1:1";
    let t_join = scenario_common::perf::now();
    let join = spawn_membership(&http, "join", peer);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let t = scenario_common::perf::now();
    assert_eq!(int_of(&r(&a, &[b"INCR", COUNTER.as_bytes()]).await), INCRS + 2, "data plane answers");
    assert_eq!(http_raw(&http, &format!("/get?key={KEY}&raft-token={TOKEN}"), five).await, (200, "v160\n".to_string()), "/get is lock-free");
    assert_eq!(http_raw(&http, &format!("/join?peerAddress={peer}&raft-token={WRONG}"), five).await, (401, "unauthorized\n".to_string()), "401 before the mux");
    let m9 = scrape(&monitor).await;
    assert_eq!(sample(&m9, INCR_HIST), Some((INCRS + 2) as f64), "{m9}");
    scenario_common::perf::within("census160/mux-held-probes", t, Duration::from_secs(2));
    let t_depart = scenario_common::perf::now();
    let depart = spawn_membership(&http, "depart", peer);
    let (status, body) = join.await.expect("join task");
    scenario_common::perf::at_least("census160/join-held", t_join, Duration::from_secs(30));
    assert_eq!((status, body.as_str()), (200, "internal error\n"), "the mux timeout answer");
    let (status, body) = depart.await.expect("depart task");
    scenario_common::perf::at_least("census160/depart-queued", t_depart, Duration::from_secs(28));
    assert_eq!((status, body.as_str()), (200, "internal error\n"), "queued behind the aborted join's uncommitted config");
    // Aftermath: still the Leader, FSM intact -- but the aborted join
    // left a JOINT config (self + dead peer) effective and uncommitted:
    // `add_learner(blocking)` returns at once (replication_lag_threshold),
    // `change_membership` then parks on the unreachable voter, so `raft
    // nodes` (effective membership) lists the dead peer as a second voter
    // (numeric node-id order, so either sequence).
    let (me, it) = (format!("{{Suffrage:Voter ID:{raft} Address:{raft}}}"), format!("{{Suffrage:Voter ID:{peer} Address:{peer}}}"));
    let nodes = rt(&a, &[b"RAFT", b"NODES"]).await;
    assert!(nodes == format!("+{raft} [Leader], nodes: [{it} {me}]") || nodes == format!("+{raft} [Leader], nodes: [{me} {it}]"), "{nodes}");
    assert_eq!(rb(&a, &[b"RAFT", b"GET", KEY.as_bytes()]).await, VALUE, "FSM value survived");
    scenario_common::perf::scenario(TAG, t0);
}
