//! Multi-store scenario #95 -- the multi-tag shopping guide on the ES
//! face: a `terms` query unions the postings of several exact tag
//! values, a bare `bool.should` list is the same OR (the should-only
//! spelling), `match` flips between operator `or` (default: any term)
//! and `and` (every term in one doc), and `terms` combines with a
//! `range` under a bool filter -- the clauses the scenario corpus
//! never sent (term/match defaults were; terms/should/operator were
//! not). One REAL node, ES front.

mod common;
mod scenario_common;

use scenario_common::{front_world, http, json_of, FrontSpec};

#[tokio::test]
async fn tag_guide_unions_terms_and_gates_the_match_operator() {
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("tags95", &spec).await;
    let es = front.node.es.clone();

    // a) The catalog index with explicit mappings.
    let (status, body) = http(
        &es,
        "PUT",
        "/catalog",
        Some(r#"{"mappings":{"properties":{
            "title":{"type":"text"},
            "tag":{"type":"keyword"},
            "price":{"type":"long"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create index: {body}");

    // b) Six items across three tags (keyword terms are exact bytes).
    let docs = [
        ("i1", r#"{"title":"wool scarf red","tag":"red","price":18}"#),
        ("i2", r#"{"title":"linen scarf blue","tag":"blue","price":25}"#),
        ("i3", r#"{"title":"canvas tote red","tag":"red","price":32}"#),
        ("i4", r#"{"title":"canvas tote navy","tag":"navy","price":40}"#),
        ("i5", r#"{"title":"silk scarf wool","tag":"blue","price":55}"#),
        ("i6", r#"{"title":"denim jacket","tag":"navy","price":68}"#),
    ];
    for (id, doc) in docs {
        let (status, body) = http(&es, "PUT", &format!("/catalog/_doc/{id}"), Some(doc)).await;
        assert_eq!(status, 201, "index {id}: {body}");
    }

    // c) terms = multi-value OR over exact tag values: red ∪ blue.
    let (_s, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"terms":{"tag":["red","blue"]}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    let mut ids: Vec<&str> = hit_ids(&v);
    ids.sort_unstable();
    assert_eq!(ids, vec!["i1", "i2", "i3", "i5"], "terms red|blue: {body}");
    assert_eq!(v["hits"]["total"]["value"], 4, "{body}");

    // d) Bare should list = the same OR spelled clause-wise.
    let (_s, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"bool":{"should":[
            {"term":{"tag":"red"}},{"term":{"tag":"blue"}}]}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    let mut ids = hit_ids(&v);
    ids.sort_unstable();
    assert_eq!(ids, vec!["i1", "i2", "i3", "i5"], "should-only OR: {body}");

    // e) match default operator `or`: any of scarf/tote matches 4 docs.
    let (_s, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"match":{"title":"scarf tote"}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    let mut ids = hit_ids(&v);
    ids.sort_unstable();
    assert_eq!(ids, vec!["i1", "i2", "i3", "i4", "i5"], "match or: {body}");

    // f) operator `and`: only docs carrying EVERY term -- the two
    //    titles that have both scarf and wool (i1, i5); the other
    //    scarfs/totes miss a term.
    let (_s, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"match":{"title":{
            "query":"scarf wool","operator":"and"}}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    let ids = hit_ids(&v);
    assert_eq!(ids, vec!["i1", "i5"], "match and: {body}");

    // g) terms under a bool filter with a range: red|blue AND cheap.
    let (_s, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"bool":{"filter":[
            {"terms":{"tag":["red","blue"]}},
            {"range":{"price":{"lte":30}}}]}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    let mut ids = hit_ids(&v);
    ids.sort_unstable();
    assert_eq!(ids, vec!["i1", "i2"], "filtered terms+range: {body}");

    // h) An empty terms value list matches nothing (not everything).
    let (_s, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"terms":{"tag":[]}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    assert_eq!(v["hits"]["total"]["value"], 0, "empty terms: {body}");
}

/// Sorted-id helper over the hits array.
fn hit_ids(v: &serde_json::Value) -> Vec<&str> {
    v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect()
}
