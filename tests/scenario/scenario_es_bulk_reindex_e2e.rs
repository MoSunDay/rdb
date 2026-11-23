//! Multi-store scenario #39 -- a commerce catalog ETL reindex: MySQL is
//! the source of truth, the ES face receives a full-feed `_bulk` NDJSON
//! shipment (created rows only, `errors:false`), a bad `update` action
//! flips the per-item flag, and a doc delete plus re-delete pins the
//! `deleted` -> `not_found` (non-404) lifecycle while `_count` and a
//! match query track the shrinking index. One REAL node, MySQL + ES.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, http, json_of, FrontSpec};

#[tokio::test]
async fn catalog_bulk_reindex_errors_flag_and_delete_lifecycle() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, a) = front_world("bulk39", &spec).await;
    let (es, _resp) = (front.node.es.clone(), a.clone());
    let mut c = connect_root(&front.node).await;

    // a) Source of truth: four catalog rows land in MySQL.
    ddl(
        &mut c,
        "CREATE TABLE catalog (id BIGINT PRIMARY KEY, title VARCHAR(64) NOT NULL, \
         tag VARCHAR(16) NOT NULL, price BIGINT NOT NULL)",
    )
    .await;
    let rows = [
        (1, "cordless drill", "tools", 8900),
        (2, "hardcover atlas", "books", 3200),
        (3, "espresso grinder", "kitchen", 7400),
        (4, "garden hose", "tools", 1500),
    ];
    for (id, title, tag, price) in rows {
        run(
            &mut c,
            &format!(
                "INSERT INTO catalog (id, title, tag, price) \
                 VALUES ({id}, '{title}', '{tag}', {price})"
            ),
        )
        .await;
    }
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM catalog").await, vec![4]);

    // b) The target index is created with the type mapping first (the
    //    bulk face never auto-creates an index).
    let mappings = r#"{"mappings":{"properties":{
        "title":{"type":"text"},"tag":{"type":"keyword"},
        "price":{"type":"long"}}}}"#;
    let (status, body) = http(&es, "PUT", "/catalog", Some(mappings)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["acknowledged"], true, "{body}");

    // c) Full-feed shipment: one NDJSON index action per row. The
    //    happy path keeps `errors:false` and every item reports
    //    `created`.
    let mut ndjson = String::new();
    for (id, title, tag, price) in rows {
        ndjson.push_str(&format!(
            "{{\"index\":{{\"_index\":\"catalog\",\"_id\":\"{id}\"}}}}\n\
             {{\"title\":\"{title}\",\"tag\":\"{tag}\",\"price\":{price}}}\n"
        ));
    }
    let (status, body) = http(&es, "POST", "/_bulk", Some(&ndjson)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], false, "{v}");
    for i in 0..4 {
        assert_eq!(v["items"][i]["index"]["result"], "created", "{v}");
        assert_eq!(v["items"][i]["index"]["status"], 201, "{v}");
    }

    // d) The index is live at four docs; a match query sees the drill.
    let (status, body) = http(&es, "POST", "/catalog/_count", Some("{}")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["count"], 4, "{body}");
    let (status, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"match":{"title":"drill"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["hits"]["total"]["value"], 1, "{body}");
    assert_eq!(json_of(&body)["hits"]["hits"][0]["_id"], "1", "{body}");

    // e) A shipment carrying an unsupported `update` action: the batch
    //    still answers 200 but the per-item error flips `errors:true`
    //    (bulk error semantics are per item, never all-or-nothing).
    let up_bulk = concat!(
        r#"{"update":{"_index":"catalog","_id":"1"}}"#,
        "\n",
        r#"{"doc":{"price":9900}}"#,
        "\n",
    );
    let (status, body) = http(&es, "POST", "/_bulk", Some(up_bulk)).await;
    assert_eq!(status, 200, "{body}");
    let v = json_of(&body);
    assert_eq!(v["errors"], true, "{v}");

    // f) Retire a product in the source of truth, then mirror the
    //    delete into the index: first delete says `deleted` (200), the
    //    re-delete says `not_found` and must NOT surface as an HTTP
    //    404 error -- idempotent erasure, not a failure.
    run(&mut c, "DELETE FROM catalog WHERE id = 2").await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM catalog").await, vec![3]);
    let (status, body) = http(&es, "DELETE", "/catalog/_doc/2", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["result"], "deleted", "{body}");
    let (status, body) = http(&es, "DELETE", "/catalog/_doc/2", None).await;
    assert_ne!(status, 404, "re-delete is not a 404: {status} {body}");
    assert_eq!(json_of(&body)["result"], "not_found", "{body}");

    // g) Index and source agree again: three docs, atlas is gone, the
    //    drill (whose bulk update failed) kept its original price.
    let (_status, body) = http(&es, "POST", "/catalog/_count", Some("{}")).await;
    assert_eq!(json_of(&body)["count"], 3, "{body}");
    let (_status, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"match":{"title":"atlas"}}}"#),
    )
    .await;
    assert_eq!(json_of(&body)["hits"]["total"]["value"], 0, "{body}");
    let (_status, body) = http(
        &es,
        "POST",
        "/catalog/_search",
        Some(r#"{"query":{"match":{"title":"drill"}}}"#),
    )
    .await;
    assert_eq!(
        json_of(&body)["hits"]["hits"][0]["_source"]["price"],
        8900,
        "{body}"
    );
}
