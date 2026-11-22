//! Multi-store scenario #96 -- the visual search showcase on the ES
//! face: a `knn` query returns nearest-first neighbors with scores in
//! (0,1], a knn `filter` clause restricts the candidate pool (vector
//! neighbors must ALSO match the filter), `_source` shaping prunes the
//! hit payloads (false -> null, includes -> only the named paths),
//! and `sort: ["_doc"]` walks the index in insertion order -- the
//! request-body faces the scenario corpus never exercised. One REAL
//! node, ES front.

mod common;
mod scenario_common;

use scenario_common::{front_world, http, json_of, FrontSpec};

#[tokio::test]
async fn visual_search_ranks_neighbors_and_shapes_the_payload() {
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("vis96", &spec).await;
    let es = front.node.es.clone();

    // a) The showcase index: 2-d dress vectors + brand keywords.
    let (status, body) = http(
        &es,
        "PUT",
        "/showcase",
        Some(r#"{"mappings":{"properties":{
            "name":{"type":"text"},
            "brand":{"type":"keyword"},
            "vec":{"type":"dense_vector","dims":2}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create index: {body}");

    // b) Six garments: vectors spread around the box, three brands.
    let docs = [
        ("v1", r#"{"name":"linen shift","brand":"atlas","vec":[0.0,0.0]}"#),
        ("v2", r#"{"name":"denim shirt","brand":"atlas","vec":[0.2,0.1]}"#),
        ("v3", r#"{"name":"wool coat","brand":"boreal","vec":[1.0,1.0]}"#),
        ("v4", r#"{"name":"silk dress","brand":"boreal","vec":[0.9,1.1]}"#),
        ("v5", r#"{"name":"rain cape","brand":"drift","vec":[3.0,3.0]}"#),
        ("v6", r#"{"name":" quilted vest","brand":"drift","vec":[3.1,2.9]}"#),
    ];
    for (id, doc) in docs {
        let (status, body) = http(&es, "PUT", &format!("/showcase/_doc/{id}"), Some(doc)).await;
        assert_eq!(status, 201, "index {id}: {body}");
    }

    // c) Bare knn: the two nearest neighbors of [0.1,0.0] come first
    //    (v1 then v2, both atlas), scores inside (0,1].
    let (_s, body) = http(
        &es,
        "POST",
        "/showcase/_search",
        Some(r#"{"knn":{"field":"vec","query_vector":[0.1,0.0],"k":2,
            "num_candidates":50}}"#),
    )
    .await;
    let v = json_of(&body);
    let ids = hit_ids(&v);
    assert_eq!(ids, vec!["v1", "v2"], "nearest first: {body}");
    for h in v["hits"]["hits"].as_array().unwrap() {
        let score = h["_score"].as_f64().unwrap();
        assert!(score > 0.0 && score <= 1.0, "score {score} in (0,1]: {body}");
    }

    // d) knn + filter: the SAME query vector but candidates restricted
    //    to boreal -- the neighbors are the boreal pair now.
    let (_s, body) = http(
        &es,
        "POST",
        "/showcase/_search",
        Some(r#"{"knn":{"field":"vec","query_vector":[0.1,0.0],"k":2,
            "num_candidates":50,"filter":{"term":{"brand":"boreal"}}}}"#),
    )
    .await;
    let v = json_of(&body);
    let mut ids = hit_ids(&v);
    ids.sort_unstable();
    assert_eq!(ids, vec!["v3", "v4"], "filter reroutes the pool: {body}");

    // e) _source shaping on the knn hits: false -> _source null;
    //    includes -> only the named path survives.
    let (_s, body) = http(
        &es,
        "POST",
        "/showcase/_search",
        Some(r#"{"knn":{"field":"vec","query_vector":[0.1,0.0],"k":1,
            "num_candidates":50},"_source":false}"#),
    )
    .await;
    let v = json_of(&body);
    assert!(v["hits"]["hits"][0]["_source"].is_null(), "source off: {body}");
    assert_eq!(v["hits"]["hits"][0]["_id"], "v1", "{body}");
    let (_s, body) = http(
        &es,
        "POST",
        "/showcase/_search",
        Some(r#"{"knn":{"field":"vec","query_vector":[3.0,3.0],"k":2,
            "num_candidates":50},"_source":["brand"]}"#),
    )
    .await;
    let v = json_of(&body);
    for h in v["hits"]["hits"].as_array().unwrap() {
        let src = &h["_source"];
        assert!(src.get("brand").is_some() && src.get("name").is_none() && src.get("vec").is_none(),
            "includes prune: {body}");
    }

    // f) sort _doc: insertion order with scoring suppressed (the
    //    _score slot reads null under a non-score sort).
    let (_s, body) = http(
        &es,
        "POST",
        "/showcase/_search",
        Some(r#"{"sort":["_doc"],"query":{"match_all":{}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    assert_eq!(hit_ids(&v), vec!["v1", "v2", "v3", "v4", "v5", "v6"], "doc order: {body}");
    assert!(v["hits"]["hits"][0]["_score"].is_null(), "score suppressed: {body}");
}

/// Hit ids in reply order.
fn hit_ids(v: &serde_json::Value) -> Vec<&str> {
    v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect()
}
