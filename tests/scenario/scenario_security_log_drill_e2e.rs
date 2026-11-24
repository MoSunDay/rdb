//! Multi-store scenario #115 -- a security-log drill on the ES face,
//! mirrored by a SQL table: 100 log docs land through one `_bulk`
//! NDJSON shipment (`errors:false`, 100 `created` items), then the
//! v2 query surface slices them four ways -- `query_string` (implicit
//! AND across two fields), `match_phrase` (positional adjacency),
//! `prefix` on a keyword field (posting-key disjunction) and
//! `_source` shaping (`includes` prunes to EXACTLY the named keys,
//! `excludes` drops the named one) -- every count compared against a
//! Rust-side tally of the very same corpus, and against
//! `SELECT COUNT(*)` over the MySQL mirror. The index lifecycle is
//! pinned with HEAD -> 200, DELETE -> 200, HEAD -> 404. One REAL
//! node, MySQL + ES.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, http, json_of, FrontSpec};
use std::time::Duration;

/// 4 hosts x 3 levels x 5 messages, pairwise coprime strides, so the
/// 100-doc corpus cycles through every combination.
const HOSTS: [&str; 4] = ["web-1", "web-2", "db-1", "cache-1"];
const LEVELS: [&str; 3] = ["info", "warn", "error"];
const MESSAGES: [&str; 5] = [
    "authentication failure detected",
    "cache eviction rate high",
    "request completed ok",
    "authentication failure detected on ssh",
    "disk usage above threshold",
];

/// One log line of the corpus: `(host, level, message)`.
fn line(i: usize) -> (&'static str, &'static str, &'static str) {
    (
        HOSTS[i % HOSTS.len()],
        LEVELS[i % LEVELS.len()],
        MESSAGES[i % MESSAGES.len()],
    )
}

/// The 100-line corpus as `(host, level, message)` triples.
fn corpus() -> Vec<(&'static str, &'static str, &'static str)> {
    (0..100).map(line).collect()
}

/// How many corpus lines satisfy `pred` (the oracle for every ES count).
fn tally(pred: impl Fn(&(&str, &str, &str)) -> bool) -> usize {
    corpus().iter().filter(|l| pred(l)).count()
}

/// Indexed document body of line `i` (epoch-millis `ts` rides the
/// numeric mapping -- see the `date` note in the test body).
fn doc_json(i: usize) -> String {
    let (host, level, message) = line(i);
    format!(
        r#"{{"message":"{message}","level":"{level}","host":"{host}","ts":{}}}"#,
        1_700_000_000_000_u64 + i as u64 * 1000
    )
}

#[tokio::test]
async fn security_log_bulk_query_and_lifecycle() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        es: true,
        mysql: true,
        ..Default::default()
    };
    let (front, _resp) = front_world("sec115", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // a) `date` is NOT an accepted mapping type on this face (numeric
    //    types only): pinned as a 400, then the index is created with
    //    `ts` as an epoch-millis `long` instead.
    let (status, body) = http(
        &es,
        "PUT",
        "/seclogs",
        Some(
            r#"{"mappings":{"properties":{"message":{"type":"text"},
            "level":{"type":"keyword"},"host":{"type":"keyword"},
            "ts":{"type":"date"}}}}"#,
        ),
    )
    .await;
    assert_eq!(status, 400, "date mapping: {body}");
    assert!(
        body.contains("unknown field type 'date'"),
        "date rejection text: {body}"
    );
    let (status, body) = http(
        &es,
        "PUT",
        "/seclogs",
        Some(
            r#"{"mappings":{"properties":{"message":{"type":"text"},
            "level":{"type":"keyword"},"host":{"type":"keyword"},
            "ts":{"type":"long"}}}}"#,
        ),
    )
    .await;
    assert_eq!(status, 200, "create seclogs: {body}");
    assert_eq!(json_of(&body)["acknowledged"], true, "{body}");

    // b) The hot block: one 100-doc `_bulk` shipment plus 20 searches
    //    (5 rounds over the four query shapes below).
    let t = scenario_common::perf::now();
    let mut ndjson = String::new();
    for i in 0..100 {
        ndjson.push_str(&format!(
            "{{\"index\":{{\"_index\":\"seclogs\",\"_id\":\"{i}\"}}}}\n{}\n",
            doc_json(i)
        ));
    }
    let (status, body) = http(&es, "POST", "/_bulk", Some(&ndjson)).await;
    assert_eq!(status, 200, "bulk: {body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], false, "{v}");
    let items = v["items"].as_array().expect("items array");
    assert_eq!(items.len(), 100, "{v}");
    for (i, item) in items.iter().enumerate() {
        assert_eq!(item["index"]["status"], 201, "item {i}: {v}");
        assert_eq!(item["index"]["result"], "created", "item {i}: {v}");
    }
    let queries = [
        // query_string: implicit AND across two fields.
        (
            r#"{"query":{"query_string":{"query":"message:authentication AND level:error"}}}"#,
            tally(|l| l.2.contains("authentication") && l.1 == "error"),
        ),
        // match_phrase: adjacent positions in the text field.
        (
            r#"{"query":{"match_phrase":{"message":"authentication failure"}}}"#,
            tally(|l| l.2.contains("authentication failure")),
        ),
        // prefix on the keyword host field (as-typed posting keys).
        (
            r#"{"query":{"prefix":{"host":"web"}}}"#,
            tally(|l| l.0.starts_with("web")),
        ),
        // match_all: the whole corpus.
        (r#"{"query":{"match_all":{}}}"#, 100),
    ];
    let mut es_total = 0u64;
    for _ in 0..5 {
        for (q, want) in queries {
            let (status, body) = http(&es, "POST", "/seclogs/_search", Some(q)).await;
            assert_eq!(status, 200, "{q}: {body}");
            let total = json_of(&body)["hits"]["total"]["value"].clone();
            assert_eq!(total, want, "{q}: {body}");
            if q.contains("match_all") {
                es_total = total.as_u64().expect("match_all total");
            }
        }
    }
    scenario_common::perf::within("sec115/100-bulk+20-search", t, Duration::from_secs(3));

    // c) `_source` shaping: includes prunes to EXACTLY the two named
    //    keys (message and ts are gone), excludes drops just message.
    let (status, body) = http(
        &es,
        "POST",
        "/seclogs/_search",
        Some(
            r#"{"_source":{"includes":["host","level"]},
            "query":{"query_string":{"query":"message:authentication"}},"size":1}"#,
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    let src = &v["hits"]["hits"][0]["_source"];
    let mut keys: Vec<&str> = src
        .as_object()
        .expect("source object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["host", "level"], "includes: {body}");
    let (status, body) = http(
        &es,
        "POST",
        "/seclogs/_search",
        Some(r#"{"_source":{"excludes":["message"]},"query":{"match_all":{}},"size":1}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    let src = &v["hits"]["hits"][0]["_source"];
    assert!(src.get("message").is_none(), "excludes: {body}");
    for kept in ["host", "level", "ts"] {
        assert!(src.get(kept).is_some(), "excludes kept {kept}: {body}");
    }

    // d) Index lifecycle: HEAD 200, DELETE 200, HEAD 404.
    let (status, body) = http(&es, "HEAD", "/seclogs", None).await;
    assert_eq!(status, 200, "head before delete: {body}");
    let (status, body) = http(&es, "DELETE", "/seclogs", None).await;
    assert_eq!(status, 200, "delete index: {body}");
    assert_eq!(json_of(&body)["acknowledged"], true, "{body}");
    let (status, body) = http(&es, "HEAD", "/seclogs", None).await;
    assert_eq!(status, 404, "head after delete: {body}");

    // e) The SQL mirror: 100 rows in, and `COUNT(*)` equals the ES
    //    total -- both for the whole corpus and for the AND slice.
    ddl(
        &mut c,
        "CREATE TABLE seclog_mirror (id BIGINT PRIMARY KEY, host VARCHAR(16) NOT NULL, \
         level VARCHAR(8) NOT NULL, message VARCHAR(64) NOT NULL, ts BIGINT NOT NULL)",
    )
    .await;
    let mut values = String::new();
    for i in 0..100 {
        let (host, level, message) = line(i);
        if i > 0 {
            values.push_str(", ");
        }
        values.push_str(&format!(
            "({i}, '{host}', '{level}', '{message}', {})",
            1_700_000_000_000_u64 + i as u64 * 1000
        ));
    }
    run(
        &mut c,
        &format!("INSERT INTO seclog_mirror VALUES {values}"),
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM seclog_mirror").await,
        vec![100]
    );
    let (status, body) = http(
        &es,
        "POST",
        "/seclogs/_search",
        Some(r#"{"query":{"match_all":{}}}"#),
    )
    .await;
    assert_eq!(status, 404, "index is gone: {body}");
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM seclog_mirror").await,
        vec![es_total as i64],
        "SQL mirror count == ES total"
    );
    let err_and = tally(|l| l.2.contains("authentication") && l.1 == "error") as i64;
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM seclog_mirror WHERE message LIKE '%authentication%' \
             AND level = 'error'"
        )
        .await,
        vec![err_and],
        "SQL mirror of the query_string slice"
    );
    scenario_common::perf::scenario("sec115", t0);
}
