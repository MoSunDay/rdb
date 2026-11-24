//! Multi-store scenario #129 -- Kafka offset/epoch algebra over a Lite
//! stream seeded from RESP: 10 XADDs on the `epoch129` parent (served
//! as topic `epoch129` p0), then ListOffsets v5 latest == 10 / earliest
//! == 0, OffsetForLeaderEpoch v2 for the live partition == (error 0,
//! leader_epoch -1 "no epochs", end offset 10) and for an unknown
//! partition == (UNKNOWN_TOPIC_OR_PARTITION 3, -1, -1); OffsetCommit v0
//! 7 -> OffsetFetch v0 7 -> Fetch v0 from 7 yields the tail batch (base
//! 7, three records, hwm 10), and a SQL watermark table mirrors the
//! committed offset against the log end. One REAL node, Kafka + RESP +
//! MySQL.

use crate::common;
use crate::kafka_front_common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, ints, one, rows as sql_rows, run};
use kafka_front_common::groups::{commit_v0, fetch_offset_v0};
use kafka_front_common::topics::{list_offsets_v5, offset_for_leader_epoch};
use kafka_front_common::{kafka_req, kafka_round};
use mysql_async::Value as MVal;
use rdb::kafka::frame::{Reader, put_array_len, put_i32, put_i64, put_string};
use scenario_common::{FrontSpec, front_world, int_of, r};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

const API_FETCH: i16 = 1;
const RECORDS: i64 = 10;
const COMMIT_AT: i64 = 7;
const UNKNOWN_TOPIC_OR_PARTITION: i16 = 3;
const GROUP: &str = "epoch129grp";

/// Fetch v0 round for (topic, partition 0, offset): (error, hwm,
/// record-set bytes).
async fn fetch_v0(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    offset: i64,
) -> (i16, i64, Vec<u8>) {
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

/// (base_offset, record_count) of a record batch (v2 batch header).
fn batch_head(records: &[u8]) -> (i64, i32) {
    assert!(records.len() >= 27, "batch header present");
    let base = i64::from_be_bytes(records[..8].try_into().unwrap());
    let last_delta = i32::from_be_bytes(records[23..27].try_into().unwrap());
    (base, last_delta + 1)
}

#[tokio::test]
async fn offsets_epochs_and_commits_line_up_with_the_fetch() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("epoch129", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Ten records on the RESP plane: XADD on the bare parent pins the
    //    implicit queue `epoch129/q0`, which the kafka front serves as
    //    topic `epoch129` partition 0.
    for i in 0..RECORDS {
        let id = r(
            &a,
            &[
                b"XADD",
                b"epoch129",
                b"*",
                b"evt",
                format!("wire-{i}").as_bytes(),
            ],
        )
        .await;
        assert!(id.starts_with(b"*2"), "xadd {i} replied an id pair: {id:?}");
    }
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"epoch129/q0"]).await),
        RECORDS,
        "stream depth"
    );

    // b) A raw consumer connects once the kafka listener accepts.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut sock = loop {
        match TcpStream::connect(&front.kafka).await {
            Ok(s) => break s,
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "kafka front never accepted; {}",
                    front.node.ctx()
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };

    // c) The hot block: ten rounds of (latest, earliest, epoch) probes
    //    over the live partition plus the unknown-partition probe.
    let t = scenario_common::perf::now();
    let mut corr = 1;
    for _ in 0..RECORDS {
        assert_eq!(
            list_offsets_v5(&mut sock, corr, "epoch129", 0, -1).await,
            (0, -1, RECORDS, -1),
            "latest = log end"
        );
        corr += 1;
        assert_eq!(
            list_offsets_v5(&mut sock, corr, "epoch129", 0, -2).await,
            (0, -1, 0, -1),
            "earliest = 0"
        );
        corr += 1;
        assert_eq!(
            offset_for_leader_epoch(&mut sock, corr, "epoch129", 0).await,
            (0, -1, RECORDS),
            "live partition: error 0, leader_epoch -1, end = log end"
        );
        corr += 1;
    }
    assert_eq!(
        offset_for_leader_epoch(&mut sock, corr, "epoch129", 7).await,
        (UNKNOWN_TOPIC_OR_PARTITION, -1, -1),
        "unknown partition"
    );
    corr += 1;
    scenario_common::perf::within("epoch129/10-epoch+10-offset", t, Duration::from_secs(3));

    // d) Commit 7 -> fetch 7 -> the resume fetch returns the tail batch.
    assert_eq!(
        commit_v0(&mut sock, corr, GROUP, "epoch129", COMMIT_AT).await,
        0,
        "commit"
    );
    corr += 1;
    assert_eq!(
        fetch_offset_v0(&mut sock, corr, GROUP, "epoch129", 0).await,
        COMMIT_AT,
        "committed offset read back"
    );
    corr += 1;
    let (error, hwm, records) = fetch_v0(&mut sock, corr, "epoch129", COMMIT_AT).await;
    corr += 1;
    assert_eq!(error, 0, "resume fetch clean");
    assert_eq!(hwm, RECORDS, "hwm = log end");
    assert_eq!(
        batch_head(&records),
        (COMMIT_AT, 3),
        "tail batch: base 7, three records"
    );
    let body = String::from_utf8_lossy(&records).into_owned();
    for i in [7, 8, 9] {
        assert!(
            body.contains(&format!("wire-{i}")),
            "record {i} rides the tail: {body}"
        );
    }
    assert!(
        !body.contains("wire-6"),
        "nothing before the commit: {body}"
    );
    // The full fetch from 0 still sees the whole log in one batch.
    let (error, hwm, records) = fetch_v0(&mut sock, corr, "epoch129", 0).await;
    assert_eq!((error, hwm), (0, RECORDS));
    assert_eq!(
        batch_head(&records),
        (0, RECORDS as i32),
        "head batch: ten records"
    );

    // e) The SQL watermark table: one row per partition, lag = log end -
    //    committed, the epoch the broker reported (-1 = none).
    ddl(
        &mut c,
        "CREATE TABLE epoch_watermark (topic VARCHAR(32) NOT NULL, partition INT NOT NULL, \
         committed_offset BIGINT NOT NULL, log_end BIGINT NOT NULL, leader_epoch INT NOT NULL, \
         lag BIGINT NOT NULL, PRIMARY KEY (topic, partition))",
    )
    .await;
    run(
        &mut c,
        &format!(
            "INSERT INTO epoch_watermark VALUES ('epoch129', 0, {COMMIT_AT}, {RECORDS}, -1, {})",
            RECORDS - COMMIT_AT
        ),
    )
    .await;
    let wm = sql_rows(
        &mut c,
        "SELECT committed_offset, log_end, leader_epoch, lag FROM epoch_watermark",
    )
    .await;
    assert_eq!(
        wm,
        vec![vec![int(COMMIT_AT), int(RECORDS), int(-1), int(3)]],
        "one partition row: commit 7, log end 10, no epoch, lag 3"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM epoch_watermark WHERE committed_offset < log_end"
        )
        .await,
        vec![1],
        "the group lags the log"
    );
    // The watermark's log end is the live stream depth on the RESP plane.
    let live_depth = int_of(&r(&a, &[b"XLEN", b"epoch129/q0"]).await);
    assert_eq!(
        one(&mut c, "SELECT log_end FROM epoch_watermark").await,
        MVal::Bytes(live_depth.to_string().into_bytes()),
        "SQL log_end == XLEN"
    );
    scenario_common::perf::scenario("epoch129", t0);
}
