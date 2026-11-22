//! Multi-store scenario #78 -- a product-review analytics desk on the
//! ES face: numeric `long` fields sort asc/desc (the default order is
//! `_score` desc, proven by a BM25-heavy match query), from/size pages
//! a window out of the full total, `_count` answers {"count":n} for
//! the filtered corpus, and from+size past 10000 refuses with the
//! verbatim Result-window error. One REAL node, ES front.

mod common;
mod scenario_common;

use scenario_common::{front_world, http, json_of, FrontSpec};

#[tokio::test]
async fn review_desk_sorts_pages_counts_and_caps_the_window() {
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("rev78", &spec).await;
    let es = front.node.es.clone();

    // a) The index needs explicit mappings first (no auto-create).
    let (status, body) = http(
        &es,
        "PUT",
        "/reviews",
        Some(r#"{"mappings":{"properties":{
            "product":{"type":"keyword"},
            "rating":{"type":"long"},
            "text":{"type":"text"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create reviews index: {body}");

    // b) Four reviews land; r1 says "great" three times (BM25 heaviest).
    let docs = [
        ("r1", r#"{"product":"phone","rating":5,"text":"great great great value"}"#),
        ("r2", r#"{"product":"phone","rating":3,"text":"great phone"}"#),
        ("r3", r#"{"product":"laptop","rating":4,"text":"solid machine"}"#),
        ("r4", r#"{"product":"phone","rating":1,"text":"great disappointment"}"#),
    ];
    for (id, doc) in docs {
        let (status, body) = http(&es, "PUT", &format!("/reviews/_doc/{id}"), Some(doc)).await;
        assert_eq!(status, 201, "index {id}: {body}");
    }

    // c) Default order is _score desc: the BM25-heaviest "great" doc
    //    leads the match query.
    let (status, body) = http(
        &es,
        "POST",
        "/reviews/_search",
        Some(r#"{"query":{"match":{"text":"great"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["hits"]["total"]["value"], 3, "{body}");
    assert_eq!(v["hits"]["hits"][0]["_id"], "r1", "score desc default: {body}");

    // d) Numeric sort asc: the 1-star review leads.
    let (_status, body) = http(
        &es,
        "POST",
        "/reviews/_search",
        Some(r#"{"sort":[{"rating":{"order":"asc"}}],"query":{"match_all":{}}}"#),
    )
    .await;
    let v = json_of(&body);
    let ids: Vec<&str> = v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["r4", "r2", "r3", "r1"], "rating asc: {body}");

    // e) The object-free string spelling and desc order: 5-star first.
    let (_status, body) = http(
        &es,
        "POST",
        "/reviews/_search",
        Some(r#"{"sort":["rating"],"query":{"match_all":{}},"size":1}"#),
    )
    .await;
    assert_eq!(json_of(&body)["hits"]["hits"][0]["_id"], "r4", "string spelling, default asc");
    let (_status, body) = http(
        &es,
        "POST",
        "/reviews/_search",
        Some(r#"{"sort":[{"rating":{"order":"desc"}}],"query":{"match_all":{}},"size":2}"#),
    )
    .await;
    let v = json_of(&body);
    let ids: Vec<&str> = v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["r1", "r3"], "rating desc top-2: {body}");

    // f) from/size paging: page 2 of the asc order keeps the full
    //    total but serves only the window.
    let (_status, body) = http(
        &es,
        "POST",
        "/reviews/_search",
        Some(r#"{"sort":[{"rating":{"order":"asc"}}],"from":1,"size":2,"query":{"match_all":{}}}"#),
    )
    .await;
    let v = json_of(&body);
    assert_eq!(v["hits"]["total"]["value"], 4, "pre-window total: {body}");
    let ids: Vec<&str> = v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["r2", "r3"], "page 2 window: {body}");

    // g) _count answers {"count":n} for the phone corpus.
    let (_status, body) = http(&es, "POST", "/reviews/_count", Some(r#"{"query":{"term":{"product":"phone"}}}"#)).await;
    let v = json_of(&body);
    assert_eq!(v["count"], 3, "{body}");
    assert!(v.get("count").is_some() && v["hits"].is_null(), "count shape: {body}");

    // h) The 10k window cap: from+size past 10000 refuses verbatim.
    let (status, body) = http(
        &es,
        "POST",
        "/reviews/_search",
        Some(r#"{"from":9999,"size":2,"query":{"match_all":{}}}"#),
    )
    .await;
    assert_eq!(status, 400, "window refused: {body}");
    let v = json_of(&body);
    assert!(
        body.contains("Result window is too large") && body.contains("[10000] but was [10001]"),
        "verbatim window error: {body}"
    );
    assert_eq!(v["error"]["type"], "illegal_argument_exception", "{body}");
}
