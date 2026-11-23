//! Multi-store scenario #109 -- the fleet-ops index lifecycle on the
//! ES face: `_cat/indices` answers the header row even on an EMPTY
//! node, a `_bulk` shipment indexes docs (created/201, errors:false),
//! a `create` action on an EXISTING id is a per-item
//! version_conflict (envelope status 400 here -- real ES says 409;
//! error.type carries the verdict) that flips errors:true, deletes settle
//! hit (`deleted`) vs miss (404 `not_found`, still errors:false), and
//! after `_refresh` the cat row reports the live docs.count. One REAL
//! node, ES front.

use crate::scenario_common;
use scenario_common::{front_world, http, json_of, FrontSpec};

#[tokio::test]
async fn index_lifecycle_cats_bulks_creates_and_deletes() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("cat109", &spec).await;
    let es = front.node.es.clone();

    // a) The EMPTY node still answers the cat table: header only.
    let (status, body) = http(&es, "GET", "/_cat/indices", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body, "health status index uuid pri rep docs.count store.size\n",
        "header-only cat on the empty node"
    );

    // b) Create the index, then bulk-index two vehicles.
    let (status, body) = http(
        &es,
        "PUT",
        "/fleetops",
        Some(r#"{"mappings":{"properties":{"vin":{"type":"keyword"},"km":{"type":"long"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create fleetops: {body}");
    let bulk = concat!(
        r#"{"index":{"_index":"fleetops","_id":"v1"}}"#,
        "\n",
        r#"{"vin":"VA-001","km":12000}"#,
        "\n",
        r#"{"index":{"_index":"fleetops","_id":"v2"}}"#,
        "\n",
        r#"{"vin":"VA-002","km":84000}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(bulk)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], false, "{v}");
    assert_eq!(v["items"][0]["index"]["status"], 201);
    assert_eq!(v["items"][0]["index"]["result"], "created");
    assert_eq!(v["items"][1]["index"]["result"], "created");

    // c) A `create` action on the EXISTING v1: a per-item error row
    //    (the item envelope reports 400; the error TYPE carries the
    //    real verdict, version_conflict_engine_exception) that flips
    //    errors:true.
    let create = concat!(
        r#"{"create":{"_index":"fleetops","_id":"v1"}}"#,
        "\n",
        r#"{"vin":"VA-001","km":0}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(create)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], true, "conflict flips the flag: {v}");
    assert_eq!(v["items"][0]["create"]["status"], 400);
    assert_eq!(
        v["items"][0]["create"]["error"]["type"],
        "version_conflict_engine_exception"
    );

    // d) Bulk deletes: the hit answers `deleted`, the miss 404
    //    `not_found` WITHOUT flipping errors:false.
    let dels = concat!(
        r#"{"delete":{"_index":"fleetops","_id":"v2"}}"#,
        "\n",
        r#"{"delete":{"_index":"fleetops","_id":"ghost"}}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(dels)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], false, "delete miss is not an error: {v}");
    assert_eq!(v["items"][0]["delete"]["result"], "deleted");
    assert_eq!(v["items"][1]["delete"]["status"], 404);
    assert_eq!(v["items"][1]["delete"]["result"], "not_found");

    // e) Refresh, then the cat row: header + ONE fleetops line with
    //    the surviving doc (v1 only -- the conflict never overwrote
    //    it, v2 is deleted).
    let (status, body) = http(&es, "POST", "/fleetops/_refresh", None).await;
    assert_eq!(status, 200, "refresh: {body}");
    let (status, body) = http(&es, "GET", "/_cat/indices", None).await;
    assert_eq!(status, 200, "{body}");
    let lines: Vec<&str> = body.trim_end().split('\n').collect();
    assert_eq!(lines.len(), 2, "header + one index row: {body}");
    assert!(lines[0].starts_with("health status index uuid pri rep docs.count store.size"));
    assert!(
        lines[1].starts_with("green open fleetops "),
        "cat row: {body}"
    );
    assert!(
        lines[1].ends_with(" 1 0 1 0b"),
        "one surviving doc: {:?}",
        lines[1]
    );
}
