//! Multi-store scenario #146 -- an on-call alert tail: producers XADD on
//! the `alerts` topic (the bare parent auto-picks queue `alerts/q0`),
//! XLEN / XRANGE / `XREAD COUNT 1` pin the first page, a blocked
//! `XREAD BLOCK 1500 ... $` on an idle tail parks to `*-1`, a reader parked
//! BEFORE an XADD wakes with exactly that entry, a consumer group fans one
//! new alert out to exactly one of two parked on-call consumers, the SAME
//! topic is Kafka `alerts` p0 (hand-framed Produce v3 acks=1 lands at the
//! log end, a hand-framed Fetch v0 sees it), MySQL keeps the alert log, and
//! a consumed tail never re-delivers.
//! Streams (XADD/XREAD/XREADGROUP) + Kafka produce/fetch + MySQL. One REAL node, fronts mysql + kafka (RESP always).
//! Deviation: reads take the FULL stream name `alerts/q0` (src/lite/entries.rs:97-99) and refuse the id `0`
//! (src/lite/model.rs:42-50 needs `ms-seq`): the brief's `STREAMS alerts 0` is pinned as its real error.

use crate::common;
use crate::kafka_front_common::{kafka_req, kafka_round};
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, rows_ordered, run, s};
use mysql_async::Value as MVal;
use rdb::kafka::frame::{Reader, put_array_len, put_i32, put_i64, put_string};
use rdb::kafka::record::BatchRecord;
use scenario_common::{FrontSpec, bulk_at, front_world, int_of, r, rt, stream_ids};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

const TOPIC: &str = "alerts";
const STREAM: &str = "alerts/q0";
const GROUP: &[u8] = b"oncall";
const API_FETCH: i16 = 1;
const KAFKA_VALUE: &[u8] = b"kafka-page-8";
/// The first page of the alert log: (severity, text).
const FIRST_PAGE: [(&str, &str); 3] = [
    ("critical", "db-cpu-92"),
    ("warning", "disk-81"),
    ("info", "deploy-done"),
];

/// Full-drain reply text with a short quiet gap (the fixture's `rf`
/// parks 250ms per call).
async fn drain(a: &str, args: &[&[u8]]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 20, 10_000).await)
}

/// XADD one alert on the bare parent topic: the reply is the
/// `[auto-picked-stream, id]` pair (src/lite/append.rs); returns the id.
async fn xadd(a: &str, sev: &str, text: &str) -> String {
    let reply = drain(
        a,
        &[
            b"XADD",
            TOPIC.as_bytes(),
            b"*",
            b"sev",
            sev.as_bytes(),
            b"text",
            text.as_bytes(),
        ],
    )
    .await;
    assert!(reply.starts_with("*2\r\n"), "xadd pair: {reply}");
    assert_eq!(bulk_at(reply.as_bytes(), 0), STREAM, "auto-picked queue: {reply}");
    let id = bulk_at(reply.as_bytes(), 1);
    assert!(id.contains('-'), "generated id: {reply}");
    id
}

/// Park one blocking read on its OWN connection (the spawned task owns
/// the socket); the full reply text rides the oneshot once answered.
/// Arrival handshake: the AUTH `+OK` is awaited BEFORE the command goes
/// out, so once this returns the server holds a live authenticated
/// session and the caller's settle sleep only has to cover the dispatch
/// of the blocking read itself (never the connect/AUTH round trip).
async fn spawn_block(addr: &str, args: &[&[u8]]) -> oneshot::Receiver<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = TcpStream::connect(addr).await.expect("block connect");
    let auth = common::lite::frame(&[b"AUTH", common::TOKEN.as_bytes()]);
    sock.write_all(&auth).await.expect("block auth write");
    let mut ok = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut ok))
        .await
        .expect("auth answered within 10s")
        .expect("auth read");
    assert_eq!(&ok, b"+OK\r\n", "auth handshake");
    sock.write_all(&common::lite::frame(args)).await.expect("block cmd write");
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut first = true;
        loop {
            let wait = if first { 10_000 } else { 20 };
            match tokio::time::timeout(Duration::from_millis(wait), sock.read(&mut chunk)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&chunk[..n]);
                    first = false;
                }
            }
        }
        let _ = tx.send(common::lite::text(&buf));
    });
    rx
}

/// A parked reader's reply, bounded so a lost wake fails loudly.
async fn answered(rx: oneshot::Receiver<String>) -> String {
    tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("blocking read answered within 5s")
        .expect("reader task alive")
}

/// Numeric `(ms, seq)` of a stream id (ids ascend numerically).
fn id_key(id: &str) -> (u64, u64) {
    let (ms, seq) = id.split_once('-').expect("ms-seq id");
    (ms.parse().unwrap(), seq.parse().unwrap())
}

/// Fetch v0 round for (topic, partition 0, offset): (error, hwm,
/// record-set bytes) -- the shape of scenario_kafka_offset_epoch_e2e.
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

/// `(base_offset, record_count)` of a v2 record batch header.
fn batch_head(records: &[u8]) -> (i64, i32) {
    assert!(records.len() >= 27, "batch header present");
    let base = i64::from_be_bytes(records[..8].try_into().unwrap());
    let last_delta = i32::from_be_bytes(records[23..27].try_into().unwrap());
    (base, last_delta + 1)
}

#[tokio::test]
async fn tail_wakes_one_reader_and_times_out_cleanly() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        kafka: true,
        ..Default::default()
    };
    let (front, a) = front_world("tail146", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE alert_log (id VARCHAR(32) NOT NULL PRIMARY KEY, sev VARCHAR(16) NOT NULL, \
         body VARCHAR(64) NOT NULL)",
    )
    .await;

    // a) The first page: three alerts on the parent topic, ids ascend,
    //    XLEN counts them, XRANGE walks them in id order and
    //    `XREAD COUNT 1` from 0-0 serves only the first entry.
    let mut ids = Vec::new();
    for (sev, text) in FIRST_PAGE {
        ids.push(xadd(&a, sev, text).await);
    }
    let keys: Vec<(u64, u64)> = ids.iter().map(|i| id_key(i)).collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "ids ascend: {ids:?}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", STREAM.as_bytes()]).await), 3, "xlen");
    let range = drain(&a, &[b"XRANGE", STREAM.as_bytes(), b"-", b"+"]).await;
    assert_eq!(stream_ids(&range), ids, "xrange ascending in id order: {range}");
    let head = drain(
        &a,
        &[b"XREAD", b"COUNT", b"1", b"STREAMS", STREAM.as_bytes(), b"0-0"],
    )
    .await;
    assert_eq!(stream_ids(&head), ids[..1], "count 1 -> first entry: {head}");
    assert!(head.contains("db-cpu-92") && !head.contains("disk-81"), "only the first: {head}");
    assert_eq!(
        rt(&a, &[b"XREAD", b"COUNT", b"1", b"STREAMS", STREAM.as_bytes(), b"0"]).await,
        "-ERR Invalid stream ID specified as stream command argument",
        "a bare `0` id is refused (ms-seq required)"
    );

    // b) An idle tail: `$` is the last id, nothing follows, so the
    //    reader parks the whole BLOCK budget and answers the nil array.
    let t = scenario_common::perf::now();
    let idle = answered(spawn_block(&a, &[b"XREAD", b"BLOCK", b"1500", b"STREAMS", STREAM.as_bytes(), b"$"]).await)
        .await;
    assert_eq!(idle, "*-1\r\n", "idle tail times out to nil");
    scenario_common::perf::at_least("tail146/idle-park", t, Duration::from_millis(1400));

    // c) A reader parked BEFORE a new XADD wakes with exactly that
    //    entry (the stream name leads the single observation). The
    //    handshake proves the session is live; the settle sleep covers
    //    the dispatch, so `$` is resolved before the XADD below.
    let t = scenario_common::perf::now();
    let reader = spawn_block(&a, &[b"XREAD", b"BLOCK", b"3000", b"STREAMS", STREAM.as_bytes(), b"$"]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let id4 = xadd(&a, "critical", "page-db-replica-lag").await;
    let woke = answered(reader).await;
    scenario_common::perf::within("tail146/parked-xadd-wake", t, Duration::from_secs(2));
    assert_eq!(stream_ids(&woke), vec![id4.clone()], "exactly the new entry: {woke}");
    assert!(
        woke.contains("page-db-replica-lag") && !woke.contains("deploy-done"),
        "no replay of consumed entries: {woke}"
    );

    // d) Fan-out: the group starts at `$`, two on-call consumers park
    //    on `>`; one new alert goes to exactly ONE of them, the other
    //    answers the nil array, and the PEL names the winner only.
    assert_eq!(
        rt(&a, &[b"XGROUP", b"CREATE", STREAM.as_bytes(), GROUP, b"$"]).await,
        "+OK"
    );
    let c1 = spawn_block(
        &a,
        &[b"XREADGROUP", b"GROUP", GROUP, b"c1", b"COUNT", b"1", b"BLOCK", b"800", b"STREAMS", STREAM.as_bytes(), b">"],
    )
    .await;
    let c2 = spawn_block(
        &a,
        &[b"XREADGROUP", b"GROUP", GROUP, b"c2", b"COUNT", b"1", b"BLOCK", b"800", b"STREAMS", STREAM.as_bytes(), b">"],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let id5 = xadd(&a, "warning", "kafka-lag-high").await;
    let (r1, r2) = (answered(c1).await, answered(c2).await);
    let winner = match (stream_ids(&r1).as_slice(), r2.as_str()) {
        ([id], "*-1\r\n") if id == &id5 => "c1",
        _ => match (r1.as_str(), stream_ids(&r2).as_slice()) {
            ("*-1\r\n", [id]) if id == &id5 => "c2",
            _ => panic!("exactly one consumer must get {id5}: c1={r1:?} c2={r2:?}"),
        },
    };
    let rows = common::lite::pel_rows(
        drain(&a, &[b"XPENDING", STREAM.as_bytes(), GROUP, b"-", b"+", b"10"])
            .await
            .as_bytes(),
    );
    assert_eq!(rows.len(), 1, "one pending row: {rows:?}");
    assert_eq!((rows[0][1].as_str(), rows[0][3].as_str()), (id5.as_str(), winner), "PEL owner");

    // e) Kafka: topic `alerts` p0 IS the stream `alerts/q0` (no `p0`
    //    queue exists, so the front maps p0 -> q0). The hand-framed
    //    Produce v3 (acks=1) lands at the log end and a hand-framed
    //    Fetch v0 from 0 sees the whole log incl. the new record.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut sock = loop {
        match TcpStream::connect(&front.kafka).await {
            Ok(s) => break s,
            Err(_) => {
                assert!(Instant::now() < deadline, "kafka front never accepted; {}", front.node.ctx());
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let t = scenario_common::perf::now();
    let base = scenario_common::kafka::kafka_produce_v3(
        &front.kafka,
        1,
        TOPIC,
        &[BatchRecord {
            timestamp_delta: 0,
            key: Some(b"alert"),
            value: Some(KAFKA_VALUE),
            headers: vec![],
        }],
    )
    .await;
    assert_eq!(base, 5, "produce base offset = log end after five XADDs");
    let (error, hwm, records) = fetch_v0(&mut sock, 2, TOPIC, 0).await;
    assert_eq!((error, hwm), (0, 6), "fetch from 0 clean, hwm = log end");
    assert_eq!(batch_head(&records), (0, 6), "one batch from the head: six records");
    let body = String::from_utf8_lossy(&records).into_owned();
    assert!(body.contains(std::str::from_utf8(KAFKA_VALUE).unwrap()), "record rides the fetch: {body}");
    let (error, _, tail) = fetch_v0(&mut sock, 3, TOPIC, i64::from(base)).await;
    assert_eq!(error, 0, "resume fetch clean");
    assert_eq!(
        batch_head(&tail),
        (i64::from(base), 1),
        "the produced record alone at its base offset"
    );
    assert_eq!(int_of(&r(&a, &[b"XLEN", STREAM.as_bytes()]).await), 6, "kafka record is a stream entry");
    scenario_common::perf::within("tail146/kafka-produce+2-fetch", t, Duration::from_secs(2));

    // f) MySQL keeps the alert log: the first page's ids/severities/
    //    texts exactly as XADD'd, COUNT = 3.
    let mut values = String::new();
    for (i, (sev, text)) in FIRST_PAGE.iter().enumerate() {
        values.push_str(&format!("{}('{}','{sev}','{text}')", if i > 0 { "," } else { "" }, ids[i]));
    }
    run(&mut c, &format!("INSERT INTO alert_log (id, sev, body) VALUES {values}")).await;
    let logged = rows_ordered(&mut c, "SELECT id, sev, body FROM alert_log").await;
    let mut want: Vec<Vec<MVal>> = FIRST_PAGE
        .iter()
        .enumerate()
        .map(|(i, (sev, text))| vec![s(&ids[i]), s(sev), s(text)])
        .collect();
    want.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    assert_eq!(logged, want, "alert_log rows == the XADD fields");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM alert_log").await, vec![3], "three alerts logged");

    // g) A consumed tail does not re-deliver: `$` again answers nil.
    let t = scenario_common::perf::now();
    let again = answered(spawn_block(&a, &[b"XREAD", b"BLOCK", b"100", b"STREAMS", STREAM.as_bytes(), b"$"]).await)
        .await;
    assert_eq!(again, "*-1\r\n", "consumed tail stays nil");
    scenario_common::perf::within("tail146/consumed-tail-reread", t, Duration::from_secs(2));
    scenario_common::perf::scenario("tail146", t0);
}
