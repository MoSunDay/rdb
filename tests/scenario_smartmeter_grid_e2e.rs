//! Multi-store scenario #30 -- a smartmeter grid across FOUR stores:
//! meters uplink through the Kafka front (Produce v3, record headers
//! ride hex-named `h` fields), the collector reads the SAME stream over
//! RESP (XRANGE mutual proof) and fans out -- per-meter kWh totals in
//! Redis (INCRBY), reading rows in MySQL for the regional GROUP BY
//! rollup, a guarded outage sweep (only meters silent for the latest
//! batch flip), and the raw batch archived byte-exact on S3. One REAL
//! node, three fronts + RESP.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use rdb::kafka::record::BatchRecord;
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{front_world, int_of, r, rb, rf, s3_call, FrontSpec};

#[tokio::test]
async fn smartmeter_grid_uplink_rollup_outage_and_archive() {
    // a) One node: Kafka + S3 + MySQL fronts together.
    let spec = FrontSpec {
        mysql: true,
        kafka: true,
        s3: true,
        ..Default::default()
    };
    let (front, a) = front_world("grid30", &spec).await;
    let (kafka, s3) = (front.kafka.clone(), front.s3.clone());
    let mut c = connect_root(&front.node).await;

    // b) The grid registry: three meters, all healthy after batch 1.
    ddl(
        &mut c,
        "CREATE TABLE meters (id VARCHAR(8) PRIMARY KEY, region VARCHAR(8), last_batch INT, status VARCHAR(8))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO meters (id, region, last_batch, status) VALUES \
         ('m1', 'north', 1, 'ok'), ('m2', 'south', 1, 'ok'), ('m3', 'north', 1, 'ok')",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE readings (id BIGINT PRIMARY KEY, meter VARCHAR(8), region VARCHAR(8), kwh INT)",
    )
    .await;
    // b0) Seed the topic's queue so Produce v3 has a stream to append to.
    let seeded = r(&a, &[b"XADD", b"meter/q0", b"1-1", b"seed", b"boot"]).await;
    assert!(scenario_common::bulk_at(&seeded, 0).contains("1-1"), "seed id echo");

    // c) Batch 2 uplinks: m1 twice, m2 once -- m3 has gone silent.
    let uplinks = [
        (r#"{"kwh":5}"#, "m1", "north"),
        (r#"{"kwh":4}"#, "m2", "south"),
        (r#"{"kwh":7}"#, "m1", "north"),
    ];
    let records: Vec<BatchRecord<'_>> = uplinks
        .iter()
        .map(|(payload, meter, region)| BatchRecord {
            timestamp_delta: 0,
            key: Some(meter.as_bytes()),
            value: Some(payload.as_bytes()),
            headers: vec![(b"region".as_slice(), Some(region.as_bytes()))],
        })
        .collect();
    let base = kafka_produce_v3(&kafka, 11, "meter", &records).await;
    assert_eq!(base, 1, "base_offset = the seed entry");

    // d) Mutual proof: the RESP side sees the SAME data the Kafka
    //    front appended, headers decoded into hex-named h fields.
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"meter/q0"]).await), 4);
    let xrange = rf(&a, &[b"XRANGE", b"meter/q0", b"-", b"+"]).await;
    for (payload, _, _) in &uplinks {
        assert!(xrange.contains(payload), "payload {payload} visible: {xrange}");
    }
    assert!(xrange.contains("726567696f6e"), "header name 'region' hex-encoded: {xrange}");

    // e) The collector drains the batch: per-meter Redis totals + SQL
    //    reading rows + the registry's last_batch watermark advances
    //    only for meters that actually spoke.
    for (i, (payload, meter, region)) in uplinks.iter().enumerate() {
        let kwh: i64 = payload[7..8].parse().unwrap(); // '{"kwh":N}' single digit
        int_of(&r(&a, &[b"INCRBY", &format!("grid:kwh:{meter}").into_bytes(), kwh.to_string().as_bytes()]).await);
        run(
            &mut c,
            &format!(
                "INSERT INTO readings (id, meter, region, kwh) VALUES ({i}, '{meter}', '{region}', {kwh})"
            ),
        )
        .await;
        run(&mut c, &format!("UPDATE meters SET last_batch = 2 WHERE id = '{meter}'")).await;
    }
    assert_eq!(rb(&a, &[b"GET", b"grid:kwh:m1"]).await, "12", "m1 5+7");
    assert_eq!(rb(&a, &[b"GET", b"grid:kwh:m2"]).await, "4", "m2 4");

    // f) The outage sweep: a guarded UPDATE flips ONLY meters whose
    //    watermark never reached batch 2 -- m3 went dark.
    run(&mut c, "UPDATE meters SET status = 'outage' WHERE last_batch < 2 AND status = 'ok'").await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM meters WHERE status = 'outage'").await,
        vec![1],
        "exactly m3 is dark"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM meters WHERE status = 'ok'").await,
        vec![2],
        "m1 + m2 still ok"
    );

    // g) Regional rollup: SQL GROUP BY reconciles with the Redis totals.
    let rollup = ints(
        &mut c,
        "SELECT SUM(kwh) FROM readings GROUP BY region ORDER BY region",
    )
    .await;
    assert_eq!(rollup, vec![12, 4], "north 12 / south 4");

    // h) The raw batch is archived byte-exact on S3 under grid/.
    let raw: Vec<u8> = uplinks
        .iter()
        .flat_map(|(p, m, _)| format!("{m}|{p}\n").into_bytes())
        .collect();
    let (status, head, _) = s3_call(&s3, "PUT", "/rdb/grid/batch-2.lines", &raw).await;
    assert_eq!(status, 200, "archive put: {head}");
    let (status, _, got) = s3_call(&s3, "GET", "/rdb/grid/batch-2.lines", b"").await;
    assert_eq!(status, 200, "archive get");
    assert_eq!(got, raw, "byte-exact archive");
    let (status, _, body) = s3_call(&s3, "GET", "/rdb?list-type=2&prefix=grid/", b"").await;
    assert_eq!(status, 200, "list");
    assert!(
        String::from_utf8_lossy(&body).contains("grid/batch-2.lines"),
        "archive listed"
    );
}
