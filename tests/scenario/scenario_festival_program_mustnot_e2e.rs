//! Multi-store scenario #89 -- a festival programming desk on the ES
//! face filtering with bool must_not: submissions NOT in the press
//! pool (bare must_not), feature films that are also NOT rejections
//! (must + must_not in one bool), and the filter-clause variant counted
//! through `_count` -- the exclusion semantics nothing else in the
//! scenario corpus pins (the bool must_not clause is #89's new face;
//! _count itself was already pinned by #78). One REAL node, ES front.
//!
//! (Slot history: this slot was drafted as a bare-SUNION board, but
//! #73 `social_union_feed` already covers bare SUNION/SINTER/SDIFF --
//! backup "SINTER bare" likewise. EXPIRETIME is unimplemented in src,
//! so the ES must_not backup took the slot.)

use crate::scenario_common;
use scenario_common::{front_world, http, json_of, FrontSpec};

#[tokio::test]
async fn festival_desk_excludes_press_and_rejections_with_must_not() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("mn89", &spec).await;
    let es = front.node.es.clone();

    // a) The submissions index needs explicit mappings first.
    let (status, body) = http(
        &es,
        "PUT",
        "/submissions",
        Some(
            r#"{"mappings":{"properties":{
            "section":{"type":"keyword"},
            "verdict":{"type":"keyword"},
            "title":{"type":"text"}}}}"#,
        ),
    )
    .await;
    assert_eq!(status, 200, "create index: {body}");

    // b) Six submissions: 2 press, 3 feature, 1 teaser; one feature
    //    and one press entry are rejections.
    let docs = [
        (
            "s1",
            r#"{"section":"feature","verdict":"accepted","title":"north by north"}"#,
        ),
        (
            "s2",
            r#"{"section":"feature","verdict":"rejected","title":"the long take"}"#,
        ),
        (
            "s3",
            r#"{"section":"feature","verdict":"accepted","title":"quiet fires"}"#,
        ),
        (
            "s4",
            r#"{"section":"press","verdict":"rejected","title":"press notes"}"#,
        ),
        (
            "s5",
            r#"{"section":"press","verdict":"accepted","title":"media kit"}"#,
        ),
        (
            "s6",
            r#"{"section":"teaser","verdict":"accepted","title":"sixty seconds"}"#,
        ),
    ];
    for (id, doc) in docs {
        let (status, body) = http(&es, "PUT", &format!("/submissions/_doc/{id}"), Some(doc)).await;
        assert_eq!(status, 201, "index {id}: {body}");
    }

    // c) Bare must_not: everyone outside the press pool -- the 4
    //    non-press docs, press exclusions gone entirely.
    let (_s, body) = http(
        &es,
        "POST",
        "/submissions/_search",
        Some(r#"{"query":{"bool":{"must_not":{"term":{"section":"press"}}}},"size":10}"#),
    )
    .await;
    let v = json_of(&body);
    let mut ids: Vec<&str> = v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["s1", "s2", "s3", "s6"], "no press: {body}");
    assert_eq!(v["hits"]["total"]["value"], 4, "{body}");

    // d) must + must_not: features that are NOT rejections.
    let (_s, body) = http(
        &es,
        "POST",
        "/submissions/_search",
        Some(
            r#"{"query":{"bool":{
            "must":{"term":{"section":"feature"}},
            "must_not":{"term":{"verdict":"rejected"}}}},"size":10}"#,
        ),
    )
    .await;
    let v = json_of(&body);
    let mut ids: Vec<&str> = v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or(""))
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["s1", "s3"], "features minus rejections: {body}");

    // e) filter + must_not counted through _count: accepted rows that
    //    are NOT press = the 3 accepted non-press submissions.
    let (_s, body) = http(
        &es,
        "POST",
        "/submissions/_count",
        Some(
            r#"{"query":{"bool":{
            "filter":{"term":{"verdict":"accepted"}},
            "must_not":{"term":{"section":"press"}}}}}"#,
        ),
    )
    .await;
    let v = json_of(&body);
    assert_eq!(v["count"], 3, "{body}");

    // f) must_not over a value nothing holds: the full table.
    let (_s, body) = http(
        &es,
        "POST",
        "/submissions/_count",
        Some(r#"{"query":{"bool":{"must_not":{"term":{"section":"nowhere"}}}}}"#),
    )
    .await;
    let v = json_of(&body);
    assert_eq!(v["count"], 6, "must_not missing value keeps all: {body}");
}
