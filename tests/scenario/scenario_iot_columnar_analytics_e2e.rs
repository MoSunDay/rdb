//! Multi-store scenario #119 -- an IoT telemetry rollup on the columnar
//! engine, archived to S3: 1000 sensor readings land through chunked
//! multi-row INSERTs into an `ENGINE=columnar` table, the aggregates
//! answer over the union of the resulting segments (`COUNT(*)`,
//! `AVG(value)` exact to the printed precision, MIN/MAX and a per-device
//! GROUP BY), the append-only contract refuses UPDATE / DELETE /
//! CREATE INDEX with MySQL 1235 and a message that names the columnar
//! table, `SHOW CREATE TABLE` renders `ENGINE=columnar`, TRUNCATE wipes
//! the table (and keeps its shape), and the aggregate report round-trips
//! through S3 byte-exact. One REAL node, MySQL + S3.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run, server_error, ER_NOT_SUPPORTED_YET};
use scenario_common::{front_world, s3_call, FrontSpec};
use std::time::Duration;

/// The fixture's pre-declared bucket.
const BUCKET: &str = "rdb";
/// Rows of the readings table and the chunk size of the INSERTs.
const ROWS: i64 = 1000;
const CHUNK: usize = 100;

/// Device of row `i` (10 devices, round robin).
fn device(i: i64) -> String {
    format!("meter-{:02}", i % 10)
}

/// Reading of row `i`: half-units, so every sum and average is exact
/// in binary floating point (no rounding to argue about).
fn value(i: i64) -> f64 {
    i as f64 / 2.0
}

/// Text of a wire cell.
fn txt(v: &mysql_async::Value) -> String {
    match v {
        mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        mysql_async::Value::NULL => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

/// `(id, device, ts, value)` tuples of rows `from..to`.
fn chunk_rows(from: i64, to: i64) -> String {
    let cells: Vec<String> = (from..to)
        .map(|i| {
            format!(
                "({i}, '{}', {}, {})",
                device(i),
                1_700_000_000_i64 + i * 60,
                value(i)
            )
        })
        .collect();
    cells.join(", ")
}

/// The expected `AVG(value)` over all rows.
fn avg_all() -> f64 {
    (0..ROWS).map(value).sum::<f64>() / ROWS as f64
}

/// The expected `AVG(value)` of one device bucket.
fn avg_device(bucket: i64) -> f64 {
    let sum: f64 = (0..ROWS).filter(|i| i % 10 == bucket).map(value).sum();
    sum / (ROWS / 10) as f64
}

/// The aggregate report body written to S3.
fn report() -> Vec<u8> {
    format!(
        "readings\nrows={ROWS}\navg={}\nmin={}\nmax={}\n",
        avg_all(),
        value(0),
        value(ROWS - 1)
    )
    .into_bytes()
}

#[tokio::test]
async fn columnar_rollup_append_only_and_truncate() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, _resp) = front_world("col119", &spec).await;
    let s3 = front.s3.clone();
    let mut c = connect_root(&front.node).await;

    // a) The columnar table.
    ddl(
        &mut c,
        "CREATE TABLE readings (id BIGINT PRIMARY KEY, device VARCHAR(16) NOT NULL, \
         ts BIGINT NOT NULL, value DOUBLE NOT NULL) ENGINE=columnar",
    )
    .await;

    // b) The hot block: 1000 rows through 10 chunked multi-row INSERTs
    //    (each its own segment) + the two headline aggregates.
    let t = scenario_common::perf::now();
    let mut from = 0;
    while from < ROWS {
        let to = (from + CHUNK as i64).min(ROWS);
        run(
            &mut c,
            &format!(
                "INSERT INTO readings (id, device, ts, value) VALUES {}",
                chunk_rows(from, to)
            ),
        )
        .await;
        from = to;
    }
    assert_eq!(
        txt(&common::mysql::one(&mut c, "SELECT COUNT(*) FROM readings").await),
        ROWS.to_string(),
        "count"
    );
    assert_eq!(
        txt(&common::mysql::one(&mut c, "SELECT AVG(value) FROM readings").await),
        avg_all().to_string(),
        "avg"
    );
    scenario_common::perf::within("col119/1000-insert+2-agg", t, Duration::from_secs(3));

    // c) The aggregates over the union of segments: count, exact
    //    average, min/max, and one average per device bucket.
    assert_eq!(
        rows(
            &mut c,
            "SELECT COUNT(*), MIN(value), MAX(value) FROM readings"
        )
        .await,
        vec![vec![
            common::mysql::int(ROWS),
            common::mysql::s(&value(0).to_string()),
            common::mysql::s(&value(ROWS - 1).to_string())
        ]],
        "count/min/max"
    );
    assert_eq!(
        rows(&mut c, "SELECT AVG(value) FROM readings").await,
        vec![vec![common::mysql::s(&avg_all().to_string())]],
        "avg precision"
    );
    let per_device = rows(
        &mut c,
        "SELECT device, COUNT(*), AVG(value) FROM readings GROUP BY device ORDER BY device",
    )
    .await;
    assert_eq!(per_device.len(), 10, "device buckets: {per_device:?}");
    for (bucket, row) in per_device.iter().enumerate() {
        assert_eq!(txt(&row[0]), format!("meter-{bucket:02}"), "{per_device:?}");
        assert_eq!(txt(&row[1]), "100", "{per_device:?}");
        assert_eq!(
            txt(&row[2]),
            avg_device(bucket as i64).to_string(),
            "{per_device:?}"
        );
    }
    // a filtered aggregate reads the same segments
    assert_eq!(
        txt(&common::mysql::one(&mut c, "SELECT COUNT(*) FROM readings WHERE value > 400").await),
        "199",
        "filtered count"
    );

    // d) Append-only: UPDATE / DELETE / CREATE INDEX are all 1235, and
    //    every message names the columnar table.
    for sql in [
        "UPDATE readings SET value = 1.0 WHERE id = 1",
        "DELETE FROM readings WHERE id = 1",
        "CREATE INDEX r_idx ON readings (device)",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
        assert!(
            e.message.contains("columnar"),
            "{sql} message must name the engine: {}",
            e.message
        );
    }
    // ... and the refusals changed nothing.
    assert_eq!(
        txt(&common::mysql::one(&mut c, "SELECT COUNT(*) FROM readings").await),
        ROWS.to_string(),
        "count after refusals"
    );

    // e) SHOW CREATE TABLE renders the engine clause.
    let created = rows(&mut c, "SHOW CREATE TABLE readings").await;
    let ddl_text = txt(&created[0][1]);
    assert!(
        ddl_text.contains("ENGINE=columnar"),
        "engine clause: {ddl_text}"
    );
    assert!(ddl_text.contains("`readings`"), "table name: {ddl_text}");

    // f) S3 archive: the aggregate report is a deterministic body and
    //    GET returns it byte-exact.
    let body = report();
    let key = format!("/{BUCKET}/iot/col119/rollup.txt");
    let (status, head, _) = s3_call(&s3, "PUT", &key, &body).await;
    assert_eq!(status, 200, "put rollup: {head}");
    let (status, head, got) = s3_call(&s3, "GET", &key, b"").await;
    assert_eq!(status, 200, "get rollup: {head}");
    assert_eq!(got, body, "byte-exact rollup");
    // the archived numbers are the ones the engine answered with
    let report_text = String::from_utf8_lossy(&got).into_owned();
    assert!(
        report_text.contains(&format!("rows={ROWS}")),
        "{report_text}"
    );
    assert!(
        report_text.contains(&format!("avg={}", avg_all())),
        "{report_text}"
    );

    // g) TRUNCATE wipes the segments but keeps the definition.
    ddl(&mut c, "TRUNCATE TABLE readings").await;
    assert_eq!(
        txt(&common::mysql::one(&mut c, "SELECT COUNT(*) FROM readings").await),
        "0",
        "count after truncate"
    );
    let created = rows(&mut c, "SHOW CREATE TABLE readings").await;
    assert!(
        txt(&created[0][1]).contains("ENGINE=columnar"),
        "engine kept after truncate"
    );
    // a fresh row lands in a fresh segment after the truncate
    run(
        &mut c,
        "INSERT INTO readings (id, device, ts, value) VALUES (1, 'meter-00', 1, 2.5)",
    )
    .await;
    assert_eq!(
        rows(&mut c, "SELECT id, device, value FROM readings").await,
        vec![vec![
            common::mysql::int(1),
            common::mysql::s("meter-00"),
            common::mysql::s("2.5")
        ]],
        "post-truncate insert"
    );
    scenario_common::perf::scenario("col119", t0);
}
