//! Multi-store scenario #159 -- a support-ticket search index whose integration script
//! is held to the ES error contract while MySQL `es_docs` stays the source of truth:
//! duplicate create 409, bad mapping / malformed document 400 (mapper_parsing_exception
//! / document_parsing_exception), protocol slips 413 (announced 64 MiB+1) / 501 (chunked)
//! / 400 (GET with body), bulk 400 at 100,001 ops or a malformed action line, per-item
//! errors (index > 255, id > 512, update), the DSL 400 family (parsing_exception,
//! x_content_parse_exception, result window) and routing (404 index_not_found /
//! no_handler_found, 405 method_not_allowed, 400 invalid_index_name). The three good
//! docs land through `_bulk` (201 created each) and agree across ES, RESP FT.SEARCH and SQL.
//! SURFACES: ES + MySQL + RESP. One REAL node, fronts es + mysql.
//! Deviation: `timeout`, `track_total_hits`, `highlight` are not parsed (ignored -> 200,
//! src/es/dsl.rs:165-215); `_refresh` is a no-op acknowledgement.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, ints, rows, run, s};
use scenario_common::{bulk_at, front_world, http, json_of, FrontSpec};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const TAG: &str = "eserr159";
const IDX: &str = "eserr159";
/// URL root of the index under test.
const ROOT: &str = "/eserr159";
/// One past the bulk admission cap (src/es/bulk.rs:33).
const OVER_CAP: usize = 100_001;
/// One past the 64 MiB body cap, as an ANNOUNCED Content-Length (http.rs:69).
const OVER_BODY: &str = "67108865";

/// `(id, title, status, votes)`: tickets 1 and 2 are refund requests.
const DOCS: [(i64, &str, &str, i64); 3] = [
    (1, "refund not received for order 501", "open", 7),
    (2, "refund duplicated on a gift card", "closed", 4),
    (3, "login loop on the mobile app", "open", 9),
];

/// The ES source of ticket `i` (also the FT document; the SQL row has
/// the same three columns).
fn doc(i: usize) -> Value {
    let (_, title, status, votes) = DOCS[i];
    json!({"title": title, "status": status, "votes": votes})
}

/// One raw HTTP/1.1 round trip with verbatim header lines -- the ES twin
/// of `http`, for requests the fixture cannot express (a chunked head, an
/// announced Content-Length with no body behind it): `(status, body)`.
async fn http_raw(es: &str, method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, String) {
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let mut raw = req.into_bytes();
    raw.extend_from_slice(body);
    let mut sock = TcpStream::connect(es).await.expect("connect es");
    sock.write_all(&raw).await.expect("write es request");
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), sock.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string())
}

/// Assert one ES error envelope (reply.rs:47-60): HTTP status, the
/// mirrored `status`, `type` == `root_cause[0].type` and a reason that
/// mirrors the root cause; returns the reason for exact-text checks.
fn es_err(what: &str, reply: (u16, String), status: u16, es_type: &str) -> String {
    let (got, body) = reply;
    let v = json_of(&body);
    let err = &v["error"];
    assert_eq!(got, status, "{what}: {body}");
    assert_eq!(v["status"].as_i64(), Some(i64::from(status)), "{what}: {body}");
    assert_eq!(
        (err["type"].as_str(), err["root_cause"][0]["type"].as_str()),
        (Some(es_type), Some(es_type)),
        "{what}: {body}"
    );
    assert_eq!(err["root_cause"][0]["reason"], err["reason"], "{what}: {body}");
    err["reason"].as_str().unwrap_or_default().to_string()
}

/// `(action, item status, error type or "")` of one `_bulk` item.
fn item_shape(item: &Value) -> (&str, i64, &str) {
    let (action, inner) = item.as_object().expect("bulk item object").iter().next().expect("one action");
    (action.as_str(), inner["status"].as_i64().unwrap_or(0), inner["error"]["type"].as_str().unwrap_or(""))
}

/// `FT.SEARCH eserr159 <args...>` as a full-drain reply text.
async fn ft(a: &str, args: &[&str]) -> String {
    let mut full = vec!["FT.SEARCH", IDX];
    full.extend_from_slice(args);
    let argv: Vec<&[u8]> = full.iter().map(|t| t.as_bytes()).collect();
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, &argv, 40, 10_000).await)
}

/// The `:total` header of an FT.SEARCH FLAT array reply.
fn ft_total(reply: &str) -> i64 {
    reply
        .lines()
        .find_map(|l| l.strip_prefix(':'))
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or_else(|| panic!("no total in {reply}"))
}

/// Sorted `_id`s of a `_search` reply.
fn hit_ids(v: &Value) -> Vec<String> {
    let mut ids: Vec<String> = v["hits"]["hits"]
        .as_array()
        .map(|a| a.iter().filter_map(|h| h["_id"].as_str().map(String::from)).collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

#[tokio::test]
async fn search_index_contract_errors_and_edges() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        es: true,
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world(TAG, &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // a) Source of truth: the three tickets as SQL rows.
    ddl(
        &mut c,
        "CREATE TABLE es_docs (id INT PRIMARY KEY, title VARCHAR(64) NOT NULL, \
         status VARCHAR(8) NOT NULL, votes INT NOT NULL)",
    )
    .await;
    for (id, title, status, votes) in DOCS {
        run(&mut c, &format!("INSERT INTO es_docs VALUES ({id}, '{title}', '{status}', {votes})")).await;
    }
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM es_docs").await, vec![3]);

    // b) Protocol refusals (http.rs:133-168), each before any routing:
    //    a chunked head is the only 501, an announced body over the cap is
    //    answered without reading a byte, a GET that carries a body is a
    //    visible 400 instead of a silently dropped one.
    let t = scenario_common::perf::now();
    let reply = http_raw(&es, "PUT", "/x", &[("Transfer-Encoding", "chunked")], b"").await;
    es_err("chunked transfer", reply, 501, "not_implemented");
    let reply = http_raw(&es, "PUT", "/x", &[("Content-Length", OVER_BODY)], b"").await;
    assert_eq!(
        es_err("announced body over 64 MiB", reply, 413, "i_o_exception"),
        "request body exceeds the maximum allowed content length"
    );
    es_err("GET with a body", http(&es, "GET", "/_cluster/health", Some("{}")).await, 400, "illegal_argument_exception");

    // c) Index lifecycle: one create, then the duplicate is a 409; a bad
    //    mapping type and a malformed document are 400s that leave nothing
    //    behind (the failed index / doc stay absent).
    let maps = r#"{"mappings":{"properties":{"title":{"type":"text"},"status":{"type":"keyword"},"votes":{"type":"long"}}}}"#;
    let (status, body) = http(&es, "PUT", ROOT, Some(maps)).await;
    assert_eq!((status, json_of(&body)["acknowledged"].as_bool()), (200, Some(true)), "create: {body}");
    es_err("duplicate create", http(&es, "PUT", ROOT, Some(maps)).await, 409, "resource_already_exists_exception");
    let bad_map = r#"{"mappings":{"properties":{"title":{"type":"weird"}}}}"#;
    assert_eq!(
        es_err("bad mapping", http(&es, "PUT", "/eserr159-bad", Some(bad_map)).await, 400, "mapper_parsing_exception"),
        "unknown field type 'weird'"
    );
    es_err("index left absent", http(&es, "GET", "/eserr159-bad", None).await, 404, "index_not_found_exception");
    assert_eq!(
        es_err("malformed document", http(&es, "PUT", &format!("{ROOT}/_doc/bad"), Some(r#"{"title": nope}"#)).await, 400, "document_parsing_exception"),
        "invalid JSON document"
    );
    let (status, _) = http(&es, "HEAD", &format!("{ROOT}/_doc/bad"), None).await;
    assert_eq!(status, 404, "the rejected document was not stored");

    // d) Routing table (router.rs): unknown index vs unknown action vs
    //    wrong method, and the two length caps on the path.
    es_err("unknown index", http(&es, "GET", "/nope159/_search", None).await, 404, "index_not_found_exception");
    es_err("unknown action", http(&es, "GET", &format!("{ROOT}/_unknown"), None).await, 404, "no_handler_found_exception");
    es_err("wrong method", http(&es, "DELETE", &format!("{ROOT}/_search"), None).await, 405, "method_not_allowed_exception");
    es_err("index name over 255", http(&es, "PUT", &format!("/{}", "i".repeat(256)), Some("{}")).await, 400, "invalid_index_name_exception");
    let long_id = format!("{ROOT}/_doc/{}", "d".repeat(513));
    es_err("doc id over 512", http(&es, "GET", &long_id, None).await, 400, "illegal_argument_exception");
    es_err("refresh a missing index", http(&es, "POST", "/nope159/_refresh", None).await, 404, "index_not_found_exception");
    scenario_common::perf::within("eserr159/error-probes", t, Duration::from_secs(2));

    // e) Bulk admission and parse errors refuse the WHOLE request (400),
    //    with the 1-based physical line number in the reason.
    let t = scenario_common::perf::now();
    let over = "{\"delete\":{\"_id\":\"x\"}}\n".repeat(OVER_CAP);
    assert_eq!(
        es_err("bulk over 100,000 ops", http(&es, "POST", "/_bulk", Some(&over)).await, 400, "illegal_argument_exception"),
        "bulk request exceeds the maximum allowed number of items (100000)"
    );
    let missing_src = format!("{}\n", json!({"index": {"_index": IDX, "_id": "9"}}));
    let parsed = [
        ("\n{\"frobnicate\":{\"_id\":\"1\"}}\n", "line 2: unknown bulk action 'frobnicate'"),
        ("not json\n", "line 1: malformed JSON action line"),
        (missing_src.as_str(), "missing source line for 'index' action"),
    ];
    for (body, reason) in parsed {
        assert_eq!(es_err(reason, http(&es, "POST", "/_bulk", Some(body)).await, 400, "illegal_argument_exception"), reason);
    }
    let (status, body) = http(&es, "HEAD", &format!("{ROOT}/_doc/9"), None).await;
    assert_eq!((status, body.as_str()), (404, ""), "a refused bulk line stores nothing");

    // f) A mixed shipment: three good docs + three per-item errors. The
    //    request is 200, `errors:true`, every item keeps its own status.
    let mut bulk = String::new();
    for (i, (id, _, _, _)) in DOCS.iter().enumerate() {
        bulk.push_str(&format!("{}\n{}\n", json!({"index": {"_index": IDX, "_id": id.to_string()}}), doc(i)));
    }
    bulk.push_str(&format!("{}\n{}\n", json!({"index": {"_index": "i".repeat(256), "_id": "x"}}), doc(0)));
    bulk.push_str(&format!("{}\n{}\n", json!({"index": {"_index": IDX, "_id": "d".repeat(513)}}), doc(0)));
    bulk.push_str(&format!("{}\n{}\n", json!({"update": {"_index": IDX, "_id": "1"}}), json!({"doc": {"votes": 99}})));
    let (status, body) = http(&es, "POST", "/_bulk", Some(&bulk)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], true, "{body}");
    let items = v["items"].as_array().expect("items array");
    assert_eq!(
        items.iter().map(item_shape).collect::<Vec<_>>(),
        [
            ("index", 201, ""),
            ("index", 201, ""),
            ("index", 201, ""),
            ("index", 400, "invalid_index_name_exception"),
            ("index", 400, "illegal_argument_exception"),
            ("update", 400, "action_request_validation_exception"),
        ],
        "{body}"
    );
    for item in &items[..3] {
        assert_eq!(item["index"]["result"], "created", "{item}");
    }
    assert_eq!(items[4]["index"]["error"]["reason"], "invalid document id", "{body}");
    assert_eq!(items[5]["update"]["error"]["reason"], "update actions are not supported in this ES subset", "{body}");
    scenario_common::perf::within("eserr159/bulk-probes", t, Duration::from_secs(3));

    // g) The DSL 400 family on `_search` (dsl.rs / opts.rs): every
    //    refusal names the offending key.
    let t = scenario_common::perf::now();
    let cases = [
        ("unknown query", r#"{"query":{"frobnicate":{}}}"#, "parsing_exception", "[unknown query type 'frobnicate']"),
        ("bool must scalar", r#"{"query":{"bool":{"must":"x"}}}"#, "parsing_exception", "[bool] must clause must be an object or array"),
        ("negative size", r#"{"size":-1}"#, "parsing_exception", "[size] must be a non-negative integer"),
        ("string from", r#"{"from":"ten"}"#, "parsing_exception", "[from] must be a non-negative integer"),
        ("sort scalar", r#"{"sort":"votes"}"#, "parsing_exception", "[sort] must be an array"),
        ("sort order", r#"{"sort":[{"votes":{"order":"up"}}]}"#, "parsing_exception", "[sort] order must be 'asc' or 'desc'"),
        ("_source number", r#"{"_source":5}"#, "parsing_exception", "[_source] must be a boolean, array or object"),
        ("_source entry", r#"{"_source":{"includes":[1]}}"#, "parsing_exception", "[_source] entries must be strings"),
        ("malformed json", "{bad", "x_content_parse_exception", "malformed JSON body"),
        ("result window", r#"{"from":9999,"size":2}"#, "illegal_argument_exception", "Result window is too large, from + size must be less than or equal to: [10000] but was [10001]."),
    ];
    for (what, body, es_type, reason) in cases {
        assert_eq!(es_err(what, http(&es, "POST", &format!("{ROOT}/_search"), Some(body)).await, 400, es_type), reason, "{what}");
    }
    scenario_common::perf::within("eserr159/dsl-probes", t, Duration::from_secs(2));

    // h) The good reads. Unparsed knobs are ignored (deviation): the
    //    match-all page is three hits with no highlight and no timeout.
    let t = scenario_common::perf::now();
    let ignored = r#"{"query":{"match_all":{}},"timeout":"1ns","track_total_hits":false,"highlight":{"fields":{"title":{}}}}"#;
    let (status, body) = http(&es, "POST", &format!("{ROOT}/_search"), Some(ignored)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!((v["timed_out"].as_bool(), v["hits"]["total"]["value"].as_i64()), (Some(false), Some(3)), "{body}");
    assert_eq!(hit_ids(&v), ["1", "2", "3"], "{body}");
    assert!(v["hits"]["hits"][0].get("highlight").is_none(), "{body}");
    let (status, body) = http(&es, "POST", &format!("{ROOT}/_refresh"), None).await;
    assert_eq!((status, json_of(&body)["_shards"]["total"].as_i64()), (200, Some(1)), "{body}");
    let (status, body) = http(&es, "GET", ROOT, None).await;
    assert_eq!((status, json_of(&body)[IDX]["mappings"]["properties"]["votes"]["type"].as_str()), (200, Some("long")), "{body}");

    // i) Agreement: each ES `_source` == the SQL row == the plan; the ES
    //    `match` on "refund" == SQL LIKE == RESP FT.SEARCH on the same
    //    index; the FT hit content is byte-for-byte the ES document.
    let mut sources: Vec<Value> = Vec::new();
    for (id, title, status, votes) in DOCS {
        let (got, body) = http(&es, "GET", &format!("{ROOT}/_doc/{id}"), None).await;
        let src = json_of(&body)["_source"].clone();
        assert_eq!((got, src["title"].as_str(), src["status"].as_str(), src["votes"].as_i64()), (200, Some(title), Some(status), Some(votes)), "doc {id}: {body}");
        assert_eq!(
            rows(&mut c, &format!("SELECT title, status, votes FROM es_docs WHERE id = {id}")).await,
            vec![vec![s(title), s(status), int(votes)]],
            "SQL row {id} == ES _source"
        );
        sources.push(src);
    }
    let (status, body) = http(&es, "GET", &format!("{ROOT}/_doc/99"), None).await;
    assert_eq!((status, json_of(&body)["found"].as_bool()), (404, Some(false)), "{body}");
    let (status, body) = http(&es, "POST", &format!("{ROOT}/_search"), Some(r#"{"query":{"match":{"title":"refund"}},"size":10}"#)).await;
    let v = json_of(&body);
    assert_eq!(status, 200, "{body}");
    let sql_refunds = ints(&mut c, "SELECT id FROM es_docs WHERE title LIKE '%refund%' ORDER BY id").await;
    assert_eq!(sql_refunds, vec![1, 2]);
    assert_eq!(hit_ids(&v), ["1", "2"], "ES match == SQL LIKE: {body}");
    assert_eq!(v["hits"]["total"]["value"].as_i64(), Some(sql_refunds.len() as i64), "{body}");
    assert_eq!(v["hits"]["hits"][0]["_source"]["votes"], sources[0]["votes"], "{body}");
    let all = ft(&a, &["*", "LIMIT", "0", "10"]).await;
    assert_eq!(ft_total(&all), 3, "FT.SEARCH * == _count == SQL COUNT: {all}");
    let mut ft_ids: Vec<String> = (0..3).map(|i| bulk_at(all.as_bytes(), i * 2)).collect();
    ft_ids.sort();
    assert_eq!(ft_ids, ["1", "2", "3"], "{all}");
    let first = (0..3).find(|i| bulk_at(all.as_bytes(), i * 2) == "1").expect("doc 1 in the FT page");
    assert_eq!(json_of(&bulk_at(all.as_bytes(), first * 2 + 1)), sources[0], "{all}");
    let refunds = ft(&a, &["refund", "LIMIT", "0", "10"]).await;
    assert_eq!(ft_total(&refunds), sql_refunds.len() as i64, "FT.SEARCH refund == ES match: {refunds}");
    assert_eq!(ft_total(&ft(&a, &["nosuchword159"]).await), 0);
    scenario_common::perf::within("eserr159/agreement", t, Duration::from_secs(2));
    scenario_common::perf::scenario(TAG, t0);
}
