//! Multi-store scenario #141 -- a logging platform rolls today's index
//! through the ES REST lifecycle (PUT with mappings, auto-id POST,
//! manual-id create then update, doc GET / HEAD, scoped `_bulk`, POST and
//! GET-verb `_search` / `_count`, root + cluster health) and records the
//! rollover facts in MySQL; the REST error contract (405 on a known path
//! with the wrong method, 404 for a missing index / doc, no auto-create)
//! is asserted alongside. SURFACES: ES + MySQL. One REAL node, ES + MySQL.
//!
//! Deviation (src/es/router.rs:124-146): the GET `_search` / `_count`
//! forms carry no body and the router never reads the query string, so a
//! GET-verb search is match_all and `?q=level:error` is IGNORED (asserted
//! as such); the error-doc count comes from the POST body search.
//! Deviation (src/es/reply.rs:128-145): a doc GET miss is 404
//! `{"found":false}`, and a missing INDEX reads as a doc miss too.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, FrontSpec};
use serde_json::Value as Json;

/// The daily index this rollover owns (dots are legal in ES names).
const INDEX: &str = "logs-2026.11.25_roll141";
/// Docs the bulk shipment carries: 2 error + 2 info lines.
const BULK_DOCS: [(&str, &str, &str, i64); 4] = [
    ("e1", "error", "api", 7),
    ("e2", "error", "worker", 8),
    ("i1", "info", "api", 9),
    ("i2", "info", "web", 10),
];

/// Wire cell as text (never unwraps a NULL).
fn txt(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::NULL => "<NULL>".to_string(),
        other => format!("{other:?}"),
    }
}

/// One log document as JSON text.
fn doc(level: &str, service: &str, msg: &str, ts: i64) -> String {
    format!(r#"{{"level":"{level}","service":"{service}","msg":"{msg}","ts":{ts}}}"#)
}

/// ES call -> (status, parsed JSON body; Null when the body is empty).
async fn es(es_addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Json) {
    let (status, out) = http(es_addr, method, path, body).await;
    let json = if out.trim().is_empty() {
        Json::Null
    } else {
        json_of(&out)
    };
    (status, json)
}

/// `hits.total.value` of a POST `_search` body.
async fn search_hits(es_addr: &str, body: &str) -> i64 {
    let (status, v) = es(es_addr, "POST", &format!("/{INDEX}/_search"), Some(body)).await;
    assert_eq!(status, 200, "{body}: {v}");
    v["hits"]["total"]["value"].as_i64().unwrap_or_else(|| panic!("no total in {v}"))
}

/// `_count` of a body (POST form).
async fn count(es_addr: &str, body: &str) -> i64 {
    let (status, v) = es(es_addr, "POST", &format!("/{INDEX}/_count"), Some(body)).await;
    assert_eq!(status, 200, "{body}: {v}");
    v["count"].as_i64().unwrap_or_else(|| panic!("no count in {v}"))
}

#[tokio::test]
async fn es_rest_lifecycle_and_errors_mirror_sql() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("roll141", &spec).await;
    let (es_addr, mut c) = (front.node.es.clone(), connect_root(&front.node).await);

    // a) The rollover creates today's index with explicit mappings
    //    (no auto-create), then reads the node identity back.
    let mappings = r#"{"mappings":{"properties":{"level":{"type":"keyword"},
        "service":{"type":"keyword"},"msg":{"type":"text"},"ts":{"type":"long"}}}}"#;
    let (status, v) = es(&es_addr, "PUT", &format!("/{INDEX}"), Some(mappings)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!((v["acknowledged"].as_bool(), v["index"].as_str()), (Some(true), Some(INDEX)), "{v}");
    let (status, root) = es(&es_addr, "GET", "/", None).await;
    assert_eq!(status, 200, "{root}");
    assert_eq!(root["cluster_name"], "rdb", "{root}");
    assert_eq!(root["tagline"], "You Know, for Search", "{root}");
    assert_eq!(root["version"]["number"], "8.11.0-rdb", "{root}");
    assert!(!root["name"].as_str().unwrap_or_default().is_empty(), "{root}");
    let (status, health) = es(&es_addr, "GET", "/_cluster/health", None).await;
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["cluster_name"], "rdb", "{health}");
    assert!(
        matches!(health["status"].as_str(), Some("green") | Some("yellow")),
        "{health}"
    );
    assert_eq!(health["timed_out"], false, "{health}");
    assert_eq!(health["number_of_nodes"], 1, "one-node world: {health}");
    assert_eq!(health["active_primary_shards"], 0, "{health}");
    let (status, got) = es(&es_addr, "GET", &format!("/{INDEX}"), None).await;
    assert_eq!(status, 200, "{got}");
    let props = &got[INDEX]["mappings"]["properties"];
    assert_eq!((props["level"]["type"].as_str(), props["msg"]["type"].as_str()), (Some("keyword"), Some("text")), "{got}");
    assert_eq!(props["ts"]["type"], "long", "numeric types read back as long: {got}");
    assert_eq!(got[INDEX]["settings"]["index"]["number_of_shards"], "1", "{got}");
    assert_eq!(got[INDEX]["aliases"], serde_json::json!({}), "{got}");

    // b) Hot block: the write + read lifecycle of the day's documents.
    let t = scenario_common::perf::now();
    let first = doc("error", "api", "boom in checkout", 1);
    let (status, v) = es(&es_addr, "POST", &format!("/{INDEX}/_doc"), Some(&first)).await;
    assert_eq!(status, 201, "auto-id create: {v}");
    assert_eq!(v["result"], "created", "{v}");
    assert_eq!(v["_index"], INDEX, "{v}");
    let auto_id = v["_id"].as_str().unwrap_or_default().to_string();
    assert!(
        auto_id.len() == 20 && auto_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "server-generated 20-hex id: {auto_id}"
    );
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_doc/{auto_id}"), None).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["found"], true, "{v}");
    assert_eq!((v["_index"].as_str(), v["_id"].as_str()), (Some(INDEX), Some(auto_id.as_str())), "{v}");
    assert_eq!(v["_source"], json_of(&first), "GET echoes the stored source");
    let (status, head) = es(&es_addr, "HEAD", &format!("/{INDEX}/_doc/{auto_id}"), None).await;
    assert_eq!(status, 200, "HEAD hit");
    assert!(head.is_null(), "HEAD carries no body: {head}");
    let (status, head) = es(&es_addr, "HEAD", &format!("/{INDEX}/_doc/nope"), None).await;
    assert_eq!(status, 404, "HEAD miss");
    assert!(head.is_null(), "HEAD miss carries no body: {head}");
    let second = doc("error", "api", "boom in checkout (retry)", 2);
    let (status, v) = es(&es_addr, "PUT", &format!("/{INDEX}/_doc/{auto_id}"), Some(&second)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["result"], "updated", "{v}");
    let manual = doc("info", "web", "rollover finished", 3);
    let path = format!("/{INDEX}/_doc/manual-1");
    let (status, v) = es(&es_addr, "POST", &path, Some(&manual)).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!((v["result"].as_str(), v["_id"].as_str()), (Some("created"), Some("manual-1")), "{v}");
    let (status, v) = es(&es_addr, "PUT", &path, Some(&manual)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["result"], "updated", "same id again is an update: {v}");
    let mut ndjson = String::new();
    for (id, level, service, ts) in BULK_DOCS {
        ndjson.push_str(&format!(r#"{{"index":{{"_id":"{id}"}}}}"#));
        ndjson.push('\n');
        ndjson.push_str(&doc(level, service, "bulk line", ts));
        ndjson.push('\n');
    }
    let (status, v) = es(&es_addr, "POST", &format!("/{INDEX}/_bulk"), Some(&ndjson)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["errors"], false, "{v}");
    let items = v["items"].as_array().expect("items array");
    assert_eq!(items.len(), BULK_DOCS.len(), "{v}");
    for (i, item) in items.iter().enumerate() {
        let it = &item["index"];
        assert_eq!((it["status"].as_i64(), it["result"].as_str()), (Some(201), Some("created")), "{v}");
        assert_eq!(it["_index"], INDEX, "scoped bulk stamps the URL index: {v}");
        assert_eq!(it["_id"], BULK_DOCS[i].0, "{v}");
    }
    let term_error = r#"{"query":{"term":{"level":"error"}},"size":100}"#;
    let error_hits = search_hits(&es_addr, term_error).await;
    let total = count(&es_addr, "{}").await;
    assert_eq!(error_hits, 3, "auto-id (updated to error), manual-1 is info, e1, e2");
    assert_eq!(count(&es_addr, term_error).await, error_hits, "_count agrees with the search");
    assert_eq!(total, 6, "1 auto-id + 1 manual + 4 bulk docs");
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_search"), None).await;
    assert_eq!(status, 200, "GET-verb search: {v}");
    assert_eq!(v["hits"]["total"]["value"], total, "GET search without a body is match_all: {v}");
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_search?q=level:error"), None).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["hits"]["total"]["value"], total, "the q= param is not read (deviation): {v}");
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_count"), None).await;
    assert_eq!((status, v["count"].as_i64()), (200, Some(total)), "GET-verb count: {v}");
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_refresh"), None).await;
    assert_eq!(status, 200, "refresh ack: {v}");
    assert_eq!(v["_shards"]["successful"], 1, "{v}");
    scenario_common::perf::within("roll141/es-lifecycle", t, Duration::from_secs(2));

    // c) The REST error contract: 405 (known path, wrong method) with
    //    the ES error envelope, 404 for a missing index / doc.
    for (method, path) in [
        ("DELETE", "/".to_string()),
        ("PUT", format!("/{INDEX}/_search")),
        ("PUT", "/_cluster/health".to_string()),
    ] {
        let (status, v) = es(&es_addr, method, &path, None).await;
        assert_eq!(status, 405, "{method} {path}: {v}");
        assert_eq!(v["error"]["type"], "method_not_allowed_exception", "{v}");
        assert_eq!(v["status"], 405, "{v}");
    }
    let (status, v) = es(&es_addr, "GET", "/missing_index", None).await;
    assert_eq!(status, 404, "{v}");
    assert_eq!(v["error"]["type"], "index_not_found_exception", "{v}");
    assert_eq!(v["error"]["reason"], "no such index [missing_index]", "{v}");
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_doc/nope"), None).await;
    assert_eq!(status, 404, "{v}");
    assert_eq!((v["found"].as_bool(), v["_id"].as_str()), (Some(false), Some("nope")), "{v}");
    let (status, v) = es(&es_addr, "GET", "/missing_index/_doc/1", None).await;
    assert_eq!((status, v["found"].as_bool()), (404, Some(false)), "{v}");
    let (status, v) = es(&es_addr, "GET", "/missing_index/_search", None).await;
    assert_eq!((status, v["error"]["type"].as_str()), (404, Some("index_not_found_exception")), "{v}");
    let (status, v) = es(&es_addr, "POST", "/missing_index/_doc", Some(&doc("info", "x", "y", 1))).await;
    assert_eq!(status, 404, "no auto-create: {v}");
    assert_eq!(v["error"]["type"], "index_not_found_exception", "{v}");
    let (status, cat) = http(&es_addr, "GET", "/_cat/indices", None).await;
    assert_eq!(status, 200, "{cat}");
    let row = cat.lines().find(|l| l.contains(INDEX)).expect("cat row for the index");
    assert!(row.contains(&format!(" 1 0 {total} 0b")), "cat docs.count == _count: {row}");

    // d) MySQL keeps the rollover record; its sums equal the ES facts.
    ddl(
        &mut c,
        "CREATE TABLE rollover_log (index_name VARCHAR(64) PRIMARY KEY, docs INT, \
         errors INT, created_at DATETIME)",
    )
    .await;
    run(
        &mut c,
        &format!(
            "INSERT INTO rollover_log VALUES ('{INDEX}', {total}, {error_hits}, \
             '2026-11-25 06:00:00')"
        ),
    )
    .await;
    let sums = rows(&mut c, "SELECT SUM(docs), SUM(errors) FROM rollover_log").await;
    assert_eq!(
        (txt(&sums[0][0]), txt(&sums[0][1])),
        (total.to_string(), error_hits.to_string()),
        "{sums:?}"
    );
    let names = rows(
        &mut c,
        &format!("SELECT index_name FROM rollover_log WHERE docs = {total} AND errors = {error_hits}"),
    )
    .await;
    assert_eq!(names.len(), 1, "{names:?}");
    let rolled = txt(&names[0][0]);
    assert_eq!(rolled, INDEX, "{names:?}");
    let (status, _) = http(&es_addr, "HEAD", &format!("/{rolled}"), None).await;
    assert_eq!(status, 200, "the SQL-recorded index is live on the ES face");

    scenario_common::perf::scenario("roll141", t0);
}
