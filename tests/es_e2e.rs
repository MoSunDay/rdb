//! Process-level e2e for the Elasticsearch-compatible HTTP frontend:
//! spawn the real `rdb` binary with `es_bind` set (no token), then
//! exercise the meta endpoints, index lifecycle, doc CRUD, mapping
//! validation errors and the `_bulk` NDJSON wire over raw HTTP/1.1
//! (shared helpers in `tests/es_common/`).

mod common;
mod es_common;

use es_common::{http, json_body, spawn_es_ready};

use common::TOKEN;

/// Books mappings reused across the tests (text + keyword + numeric +
/// dense_vector, the full type matrix the ES front maps).
const MAPPINGS: &str = r#"{"mappings":{"properties":{
"title":{"type":"text"},"tag":{"type":"keyword"},
"price":{"type":"long"},"vec":{"type":"dense_vector","dims":2}}}}"#;

#[tokio::test]
async fn front_meta_and_index_lifecycle() {
    let (_node, es) = spawn_es_ready("meta").await;

    // Root + health + cat: the unauthenticated meta surface.
    let (status, body) = http(&es, "GET", "/", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["tagline"], "You Know, for Search");
    let (status, body) = http(&es, "GET", "/_cluster/health", None).await;
    assert_eq!(status, 200, "{body}");
    let health = json_body(&body);
    assert!(
        health["status"] == "green" || health["status"] == "yellow",
        "{health}"
    );
    let (status, _) = http(&es, "GET", "/_cat/indices", None).await;
    assert_eq!(status, 200);

    // Index create / duplicate / head / get / missing.
    let (status, body) = http(&es, "PUT", "/books", Some(MAPPINGS)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["acknowledged"], true);
    let (status, _) = http(&es, "PUT", "/books", Some(MAPPINGS)).await;
    assert_eq!(status, 409, "duplicate create");
    let (status, body) = http(&es, "HEAD", "/books", None).await;
    assert_eq!(status, 200);
    assert!(body.is_empty(), "HEAD must not carry a body: {body:?}");
    let (status, _) = http(&es, "HEAD", "/nope", None).await;
    assert_eq!(status, 404);
    let (status, body) = http(&es, "GET", "/books", None).await;
    assert_eq!(status, 200, "{body}");
    let props = &json_body(&body)["books"]["mappings"]["properties"];
    assert_eq!(props["title"]["type"], "text");
    assert_eq!(props["price"]["type"], "long");
    assert_eq!(props["vec"]["dims"], 2);
    let (status, body) = http(&es, "GET", "/nope", None).await;
    assert_eq!(status, 404, "{body}");
    assert!(json_body(&body)["error"]["type"] == "index_not_found_exception");

    // Mapping validation: unsupported ES type and bad dense_vector dims.
    let (status, body) = http(
        &es,
        "PUT",
        "/badmap",
        Some(r#"{"mappings":{"properties":{"x":{"type":"geo_point"}}}}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = http(
        &es,
        "PUT",
        "/books/_doc/9",
        Some(r#"{"title":"v","vec":[1,2,3]}"#),
    )
    .await;
    assert_eq!(status, 400, "wrong vector dims: {body}");

    // One doc so _count/_cat see the index as live, then the drop.
    let (status, _) = http(
        &es,
        "PUT",
        "/books/_doc/1",
        Some(r#"{"title":"hello redis","tag":"red","price":5,"vec":[0,0]}"#),
    )
    .await;
    assert_eq!(status, 201);
    let (status, body) = http(&es, "POST", "/books/_count", Some("{}")).await;
    assert_eq!(status, 200, "{body}");
    assert!(json_body(&body)["count"].as_u64().unwrap() >= 1);
    let (status, _) = http(&es, "POST", "/books/_refresh", None).await;
    assert_eq!(status, 200);
    let (status, body) = http(&es, "GET", "/_cat/indices", None).await;
    assert_eq!(status, 200);
    assert!(body.contains("books"), "_cat/indices: {body}");

    let (status, body) = http(&es, "DELETE", "/books", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["acknowledged"], true);
    let (status, _) = http(&es, "GET", "/books", None).await;
    assert_eq!(status, 404);
    let (status, _) = http(&es, "DELETE", "/books", None).await;
    assert_eq!(status, 404, "drop a missing index");
}

#[tokio::test]
async fn docs_crud_and_bulk_wire() {
    let (_node, es) = spawn_es_ready("docs").await;
    let (status, _) = http(&es, "PUT", "/books", Some(MAPPINGS)).await;
    assert_eq!(status, 200);

    // create -> 201, replace -> 200, auto-id -> 201 + fresh _id.
    let (status, body) = http(
        &es,
        "PUT",
        "/books/_doc/1",
        Some(r#"{"title":"first","tag":"a","price":1,"vec":[0,0]}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(json_body(&body)["result"], "created");
    let (status, body) = http(
        &es,
        "PUT",
        "/books/_doc/1",
        Some(r#"{"title":"second","tag":"a","price":1,"vec":[0,0]}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["result"], "updated");
    let (status, body) = http(
        &es,
        "POST",
        "/books/_doc",
        Some(r#"{"title":"auto","tag":"b","price":2,"vec":[1,1]}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert!(!json_body(&body)["_id"].as_str().unwrap_or("").is_empty());

    // op_type=create on an existing id -> 409 (no optimistic concurrency).
    let (status, body) = http(
        &es,
        "PUT",
        "/books/_doc/1?op_type=create",
        Some(r#"{"title":"x","tag":"a","price":1,"vec":[0,0]}"#),
    )
    .await;
    assert_eq!(status, 409, "{body}");

    // GET / HEAD / miss / delete / re-delete.
    let (status, body) = http(&es, "GET", "/books/_doc/1", None).await;
    assert_eq!(status, 200, "{body}");
    let doc = json_body(&body);
    assert_eq!(doc["found"], true);
    assert_eq!(doc["_source"]["title"], "second");
    let (status, _) = http(&es, "HEAD", "/books/_doc/1", None).await;
    assert_eq!(status, 200);
    let (status, body) = http(&es, "GET", "/books/_doc/zz", None).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(json_body(&body)["found"], false);
    let (status, body) = http(&es, "DELETE", "/books/_doc/1", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["result"], "deleted");
    let (status, body) = http(&es, "DELETE", "/books/_doc/1", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["result"], "not_found");

    // Type-mismatched doc: keyword field = object -> 400.
    let (status, body) = http(
        &es,
        "PUT",
        "/books/_doc/bad",
        Some(r#"{"title":"t","tag":{"nested":true},"price":1,"vec":[0,0]}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // _bulk: happy path (index + delete-miss) keeps errors:false; the
    // delete miss reports result not_found WITHOUT flipping the flag.
    let ok_bulk = concat!(
        r#"{"index":{"_index":"books","_id":"b1"}}"#,
        "\n",
        r#"{"title":"bulk one","tag":"b","price":3,"vec":[0,1]}"#,
        "\n",
        r#"{"delete":{"_index":"books","_id":"missing"}}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(ok_bulk)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_body(&body);
    assert_eq!(v["errors"], false, "{v}");
    assert_eq!(v["items"][0]["index"]["result"], "created");
    assert_eq!(v["items"][1]["delete"]["result"], "not_found");

    // _bulk carrying an `update` action: per-item error, errors:true.
    let up_bulk = concat!(
        r#"{"update":{"_index":"books","_id":"b1"}}"#,
        "\n",
        r#"{"doc":{"price":9}}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(up_bulk)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_body(&body);
    assert_eq!(v["errors"], true, "{v}");
    assert!(
        v["items"][0]["update"]["error"]["type"].is_string(),
        "update item error: {v}"
    );

    // Index-scoped POST /{index}/_bulk (no _index in the action lines).
    let scoped = concat!(
        r#"{"index":{"_id":"b2"}}"#,
        "\n",
        r#"{"title":"scoped","tag":"c","price":4,"vec":[1,0]}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/books/_bulk", Some(scoped)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["errors"], false);

    // Malformed NDJSON tail -> whole request 400.
    let bad = concat!(
        r#"{"index":{"_index":"books","_id":"b3"}}"#,
        "\n",
        r#"{"title":"ok","tag":"c","price":5,"vec":[1,1]}"#,
        "\n",
        "not-json\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(bad)).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("malformed"), "{body}");
}

/// RESP AUTH smoke on the ES-enabled node's resp port: the config the
/// helper writes must keep the data plane intact (used by the interop
/// test in es_search_e2e.rs; kept here as a cheap config regression).
#[tokio::test]
async fn es_node_keeps_resp_plane() {
    let (mut node, _es) = spawn_es_ready("resp").await;
    let r = common::cmd_one_shot(&node.resp, TOKEN, &[b"ping"]).await;
    let r = String::from_utf8_lossy(&r);
    assert!(r.starts_with("+PONG"), "ping: {r}");
    node.kill_now();
}
