//! Multi-store scenario #140 -- a BI vendor ships StarRocks-flavored DDL
//! (`ENGINE = OLAP`, `DUPLICATE KEY` / `PRIMARY KEY` models, `DISTRIBUTED
//! BY HASH(..) BUCKETS n`) via MySQL; SHOW CREATE echoes the NORMALIZED
//! clauses (which re-parse to the same shape), DESC / SHOW TABLES /
//! aggregates answer, and the BI mirror pushes the parsed catalog facts
//! into ES. SURFACES: MySQL + ES. One REAL node, MySQL + ES fronts.
//! Deviations: only DUPLICATE / PRIMARY KEY models parse (UNIQUE / AGGREGATE
//! / PROPERTIES / PARTITION BY are 1235, starrocks.rs:161-177); SHOW CREATE
//! renders just the FIRST dup-key column and the distribution of DUPLICATE
//! tables only, ENGINE=OLAP -> columnar/InnoDB, no COMMENT (show.rs:163-176,
//! translate.rs:662-665); INSERT ... SELECT into a columnar table is 1235
//! (write.rs:57-70); SHOW TABLES STATUS is 1064; no information_schema.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{
    connect_root, ddl, one, rows, run, server_error, ER_NOT_SUPPORTED_YET, ER_PARSE_ERROR,
};
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, FrontSpec};
use serde_json::Value as Json;

/// Wire cell as text (NULL spelled out; never unwraps a NULL).
fn txt(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::NULL => "<NULL>".to_string(),
        other => format!("{other:?}"),
    }
}

/// `SHOW CREATE TABLE <table>`: the render (column 2 of the one row).
async fn show_create(c: &mut mysql_async::Conn, table: &str) -> String {
    let r = rows(c, &format!("SHOW CREATE TABLE {table}")).await;
    assert_eq!(r.len(), 1, "one row for {table}: {r:?}");
    assert_eq!(txt(&r[0][0]), table, "Table column");
    txt(&r[0][1])
}

/// `SELECT COUNT(*) FROM <table>` as a number.
async fn row_count(c: &mut mysql_async::Conn, table: &str) -> i64 {
    txt(&one(c, &format!("SELECT COUNT(*) FROM {table}")).await)
        .parse()
        .unwrap_or_else(|e| panic!("count of {table}: {e}"))
}

/// Catalog fact the BI mirror keeps per table, parsed from the render:
/// (table, key_model, engine, buckets (0 = not echoed), row_count).
fn fact_of(table: &str, render: &str, row_count: i64) -> (String, String, String, i64, i64) {
    let key_model = if render.contains("DUPLICATE KEY(") {
        "duplicate"
    } else {
        "primary_key"
    };
    let engine = if render.contains("ENGINE=columnar") {
        "columnar"
    } else {
        "innodb"
    };
    let buckets = render
        .split("BUCKETS ")
        .nth(1)
        .and_then(|rest| rest.split(|ch: char| !ch.is_ascii_digit()).next())
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(0);
    (table.to_string(), key_model.to_string(), engine.to_string(), buckets, row_count)
}

/// ES call that must succeed (200 / 201) -> parsed JSON body.
async fn es_ok(es: &str, method: &str, path: &str, body: Option<&str>) -> Json {
    let (status, out) = http(es, method, path, body).await;
    assert!(status == 200 || status == 201, "{method} {path} -> {status}: {out}");
    json_of(&out)
}

/// `hits.total.value` of a POST `_search`.
async fn hits(es: &str, index: &str, body: &str) -> i64 {
    let v = es_ok(es, "POST", &format!("/{index}/_search"), Some(body)).await;
    v["hits"]["total"]["value"]
        .as_i64()
        .unwrap_or_else(|| panic!("no hits.total in {v}"))
}

/// Sorted `_id`s of a POST `_search` (size 100).
async fn hit_ids(es: &str, index: &str, body: &str) -> Vec<String> {
    let v = es_ok(es, "POST", &format!("/{index}/_search?size=100"), Some(body)).await;
    let mut ids: Vec<String> = v["hits"]["hits"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|h| h["_id"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

#[tokio::test]
async fn starrocks_ddl_round_trips_show_create() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("srs140", &spec).await;
    let (es, mut c) = (front.node.es.clone(), connect_root(&front.node).await);

    // a) The vendor's plain MySQL staging table feeds the PK-model
    //    dimension through INSERT ... SELECT.
    ddl(
        &mut c,
        "CREATE TABLE vendor_stage (id BIGINT PRIMARY KEY, code INT, name VARCHAR(32), \
         budget DECIMAL(12,2))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO vendor_stage VALUES (1, 10, 'emea', 1200.50), (2, 20, 'apac', 800.25), \
         (3, 30, 'amer', 950.00)",
    )
    .await;

    // b) The accepted StarRocks models, verbatim vendor DDL: an
    //    append-only DUPLICATE KEY fact table, a PK-model upsert
    //    dimension (DECIMAL allowed on the row engine), and a second
    //    DUPLICATE table that omits BUCKETS (server default).
    ddl(
        &mut c,
        "CREATE TABLE sr_events (event_time DATETIME NOT NULL COMMENT 'event ts', \
         region VARCHAR(16) NOT NULL, units INT NOT NULL, note VARCHAR(64) NULL) \
         DUPLICATE KEY(event_time, region) DISTRIBUTED BY HASH(region) BUCKETS 8 \
         ENGINE = OLAP",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE sr_regions (region_code INT NOT NULL, region_name VARCHAR(32) NOT NULL, \
         manager VARCHAR(32) NULL, budget DECIMAL(12,2) NULL) \
         PRIMARY KEY(region_code) DISTRIBUTED BY HASH(region_code) BUCKETS 4 \
         ENGINE = OLAP COMMENT = 'vendor dimension'",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE sr_traffic (hit_date DATE, path VARCHAR(64), hits INT) \
         DUPLICATE KEY(hit_date, path) DISTRIBUTED BY HASH(path) ENGINE = OLAP",
    )
    .await;

    // c) The rejected vendor clauses / types are loud 1235s naming
    //    the clause (never a silent drop, never a generic 1064).
    for (sql, needle) in [
        ("CREATE TABLE sr_agg (k INT, v INT) AGGREGATE KEY(k) DISTRIBUTED BY HASH(k)", "AGGREGATE KEY model"),
        ("CREATE TABLE sr_uniq (k INT, v INT) UNIQUE KEY(k) DISTRIBUTED BY HASH(k)", "UNIQUE KEY model"),
        ("CREATE TABLE sr_props (k INT, v INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) \
         PROPERTIES(\"replication_num\" = \"1\")", "PROPERTIES"),
        ("CREATE TABLE sr_part (k INT, v INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k) PARTITION BY (k)", "PARTITION BY"),
        ("CREATE TABLE sr_dec (k INT, v DECIMAL(10,2)) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k)", "DECIMAL columns are not supported on columnar"),
        ("CREATE TABLE sr_large (k LARGEINT, v INT) PRIMARY KEY(k) DISTRIBUTED BY HASH(k)", "column type LARGEINT"),
        ("CREATE TABLE sr_hll (k INT, v HLL) PRIMARY KEY(k) DISTRIBUTED BY HASH(k)", "column type HLL"),
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {} {}", e.code, e.message);
        assert!(e.message.contains(needle), "{sql}: {}", e.message);
    }
    let e = server_error(&mut c, "SHOW TABLES STATUS").await;
    assert_eq!(e.code, ER_PARSE_ERROR, "{}", e.message);
    assert!(e.message.contains("STATUS"), "{}", e.message);

    // d) Rows: VALUES into the append-only fact tables (a repeated
    //    key is NOT deduplicated), INSERT ... SELECT into the upsert
    //    dimension (the same pk twice -> one row, last values win).
    run(
        &mut c,
        "INSERT INTO sr_events VALUES ('2026-11-25 06:00:00', 'emea', 10, NULL), \
         ('2026-11-25 06:00:00', 'emea', 15, 'retry'), ('2026-11-25 06:05:00', 'apac', 7, NULL), \
         ('2026-11-25 07:00:00', 'amer', 3, 'low')",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO sr_traffic VALUES ('2026-11-24', '/', 100), ('2026-11-25', '/', 140), \
         ('2026-11-25', '/api', 60)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO sr_regions (region_code, region_name, budget) \
         SELECT code, name, budget FROM vendor_stage WHERE id <= 2",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO sr_regions (region_code, region_name, budget) \
         SELECT code, 'apac-ops', budget FROM vendor_stage WHERE id = 2",
    )
    .await;
    let e = server_error(
        &mut c,
        "INSERT INTO sr_events (event_time, region, units) \
         SELECT '2026-11-25 08:00:00', 'emea', 1 FROM vendor_stage WHERE id = 1",
    )
    .await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    assert!(e.message.contains("append-only"), "{}", e.message);

    // e) Aggregates: the fact table kept both 'emea' rows (COUNT 4,
    //    SUM 35), the dimension upserted apac (COUNT 2, new name).
    let agg = rows(&mut c, "SELECT COUNT(*), SUM(units) FROM sr_events").await;
    assert_eq!((txt(&agg[0][0]), txt(&agg[0][1])), ("4".into(), "35".into()), "{agg:?}");
    let by_region = rows(&mut c, "SELECT region, COUNT(*) FROM sr_events GROUP BY region ORDER BY region").await;
    let got: Vec<(String, String)> = by_region.iter().map(|r| (txt(&r[0]), txt(&r[1]))).collect();
    assert_eq!(
        got,
        vec![("amer".into(), "1".into()), ("apac".into(), "1".into()), ("emea".into(), "2".into())],
        "{by_region:?}"
    );
    assert_eq!(row_count(&mut c, "sr_traffic").await, 3);
    assert_eq!(row_count(&mut c, "sr_regions").await, 2, "upsert dimension");
    assert_eq!(
        txt(&one(&mut c, "SELECT region_name FROM sr_regions WHERE region_code = 20").await),
        "apac-ops",
        "second INSERT ... SELECT replaced the row"
    );
    assert_eq!(
        txt(&one(&mut c, "SELECT SUM(budget) FROM sr_regions").await),
        "2000.75",
        "DECIMAL survives the PK-model table"
    );

    // f) DESC: declared order, engine type names, DUPLICATE model
    //    keeps declared nullability and marks only the first key
    //    column PRI (recorded metadata); PK model forces NOT NULL.
    let desc = rows(&mut c, "DESC sr_events").await;
    let shape: Vec<(String, String, String, String)> = desc
        .iter()
        .map(|r| (txt(&r[0]), txt(&r[1]), txt(&r[2]), txt(&r[3])))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("event_time".into(), "datetime".into(), "NO".into(), "PRI".into()),
            ("region".into(), "varchar".into(), "NO".into(), "".into()),
            ("units".into(), "bigint".into(), "NO".into(), "".into()),
            ("note".into(), "varchar".into(), "YES".into(), "".into()),
        ],
        "{desc:?}"
    );
    let cols = rows(&mut c, "SHOW COLUMNS FROM sr_regions").await;
    let shape: Vec<Vec<String>> = cols.iter().map(|r| (0..4).map(|i| txt(&r[i])).collect()).collect();
    assert_eq!(
        shape,
        [
            ["region_code", "bigint", "NO", "PRI"],
            ["region_name", "varchar", "NO", ""],
            ["manager", "varchar", "YES", ""],
            ["budget", "decimal(12,2)", "YES", ""],
        ]
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect::<Vec<Vec<String>>>(),
        "{cols:?}"
    );

    // g) SHOW TABLES lists the vendor tables next to the staging one.
    let tables: Vec<String> = rows(&mut c, "SHOW TABLES")
        .await
        .iter()
        .map(|r| txt(&r[0]))
        .collect();
    for t in ["vendor_stage", "sr_events", "sr_regions", "sr_traffic"] {
        assert!(tables.contains(&t.to_string()), "{t} in {tables:?}");
    }

    // h) Hot block: the three SHOW CREATE renders, the round trip of
    //    the fact-table render under a new name, and the ES mirror.
    let t = scenario_common::perf::now();
    let (r_events, r_regions, r_traffic) = (
        show_create(&mut c, "sr_events").await,
        show_create(&mut c, "sr_regions").await,
        show_create(&mut c, "sr_traffic").await,
    );
    for frag in [
        "CREATE TABLE `sr_events` (",
        "`event_time` datetime NOT NULL,",
        "`note` varchar NULL DEFAULT NULL",
        ") DUPLICATE KEY(`event_time`) DISTRIBUTED BY HASH(`region`) BUCKETS 8 ENGINE=columnar",
    ] {
        assert!(r_events.contains(frag), "{frag:?} in:\n{r_events}");
    }
    assert!(!r_events.contains("OLAP") && !r_events.contains("COMMENT"), "{r_events}");
    for frag in [
        "CREATE TABLE `sr_regions` (",
        "`region_code` bigint NOT NULL,",
        "`budget` decimal(12,2) NULL DEFAULT NULL",
        "  PRIMARY KEY (`region_code`)",
        ") ENGINE=InnoDB",
    ] {
        assert!(r_regions.contains(frag), "{frag:?} in:\n{r_regions}");
    }
    assert!(
        !r_regions.contains("DISTRIBUTED BY"),
        "PK-model render keeps the distribution out of the text:\n{r_regions}"
    );
    assert!(
        r_traffic.contains(") DUPLICATE KEY(`hit_date`) DISTRIBUTED BY HASH(`path`) BUCKETS 10 ENGINE=columnar"),
        "{r_traffic}"
    );
    let round_trip = r_events.replace("CREATE TABLE `sr_events`", "CREATE TABLE `sr_events_rt`");
    ddl(&mut c, &round_trip).await;
    assert_eq!(show_create(&mut c, "sr_events_rt").await, round_trip, "render re-parses to itself");

    let facts = [
        fact_of("sr_events", &r_events, row_count(&mut c, "sr_events").await),
        fact_of("sr_regions", &r_regions, row_count(&mut c, "sr_regions").await),
        fact_of("sr_traffic", &r_traffic, row_count(&mut c, "sr_traffic").await),
    ];
    let idx = "srs_srs140";
    es_ok(
        &es,
        "PUT",
        &format!("/{idx}"),
        Some(r#"{"mappings":{"properties":{"table":{"type":"keyword"},
            "key_model":{"type":"keyword"},"engine":{"type":"keyword"},
            "buckets":{"type":"integer"},"row_count":{"type":"integer"}}}}"#),
    )
    .await;
    for (table, model, engine, buckets, rows_n) in &facts {
        let doc = format!(
            r#"{{"table":"{table}","key_model":"{model}","engine":"{engine}","buckets":{buckets},"row_count":{rows_n}}}"#
        );
        let v = es_ok(&es, "POST", &format!("/{idx}/_doc/{table}"), Some(&doc)).await;
        assert_eq!(v["result"], "created", "{doc}: {v}");
    }
    let mirror = es_ok(&es, "GET", &format!("/{idx}"), None).await;
    assert_eq!(mirror[idx]["mappings"]["properties"]["table"]["type"], "keyword", "{mirror}");
    assert_eq!(mirror[idx]["mappings"]["properties"]["buckets"]["type"], "long", "{mirror}");
    assert_eq!(mirror[idx]["settings"]["index"]["number_of_shards"], "1", "{mirror}");
    let of_model = |m: &str| -> Vec<String> {
        facts.iter().filter(|f| f.1 == m).map(|f| f.0.clone()).collect()
    };
    let (dup, pk) = (of_model("duplicate"), of_model("primary_key"));
    assert_eq!(dup.len(), 2, "two DUPLICATE tables on the SQL side");
    let q = |f: &str, v: &str| format!(r#"{{"query":{{"term":{{"{f}":"{v}"}}}}}}"#);
    assert_eq!(hit_ids(&es, idx, &q("key_model", "duplicate")).await, dup, "{dup:?}");
    assert_eq!(hit_ids(&es, idx, &q("key_model", "primary_key")).await, pk, "{pk:?}");
    assert_eq!(hits(&es, idx, &q("key_model", "duplicate")).await, dup.len() as i64);
    assert_eq!(hits(&es, idx, &q("engine", "innodb")).await, pk.len() as i64);
    let expect_b10 = facts.iter().filter(|f| f.3 == 10).count() as i64;
    assert_eq!(
        hits(&es, idx, r#"{"query":{"term":{"buckets":10}}}"#).await,
        expect_b10,
        "default bucket count surfaced through SHOW CREATE"
    );
    let expect_ge3 = facts.iter().filter(|f| f.4 >= 3).count() as i64;
    assert_eq!(
        hits(&es, idx, r#"{"query":{"range":{"row_count":{"gte":3}}}}"#).await,
        expect_ge3,
        "mirror row_count == SQL COUNT(*)"
    );
    assert_eq!(es_ok(&es, "POST", &format!("/{idx}/_count"), Some("{}")).await["count"], 3);
    scenario_common::perf::within("srs140/show-create-roundtrip", t, Duration::from_secs(2));

    scenario_common::perf::scenario("srs140", t0);
}
