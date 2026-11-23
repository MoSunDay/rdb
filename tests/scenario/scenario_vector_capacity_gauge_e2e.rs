//! Multi-store scenario #68 -- a vector index capacity gauge: VADD
//! seeds a 4-D family, VDIM and VCARD report the fixed width vs the
//! live element count, VSETATTR/VGETATTR carry per-element routing
//! tags (nil when unset), VSIM ranks the nearest with VALUES probes,
//! and VREM shrinks only the count, never the dimensionality. The ES
//! mirror serves the dashboard: a range query prices the band and a
//! size-capped search reads exactly one row. One REAL node, RESP + ES.

use crate::scenario_common;
use scenario_common::{bulk_at, front_world, http, int_of, json_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn index_gauge_similarity_and_price_band() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        es: true,
        ..Default::default()
    };
    let (front, a) = front_world("vector68", &spec).await;
    let es = front.node.es.clone();

    // a) Three 4-D embeddings land; the gauge reads dim=4, card=3.
    for (m, v) in [
        ("a", &["1", "0", "0", "0"]),
        ("b", &["0", "1", "0", "0"]),
        ("c", &["0", "0", "1", "0"]),
    ] {
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[
                        b"VADD",
                        b"{vg}emb",
                        b"VALUES",
                        b"4",
                        m.as_bytes(),
                        v[0].as_bytes(),
                        v[1].as_bytes(),
                        v[2].as_bytes(),
                        v[3].as_bytes()
                    ]
                )
                .await
            ),
            1,
            "VADD {m}"
        );
    }
    assert_eq!(int_of(&r(&a, &[b"VDIM", b"{vg}emb"]).await), 4);
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"{vg}emb"]).await), 3);

    // b) Routing tags ride elements: set, read back, nil for the rest.
    assert_eq!(
        int_of(&r(&a, &[b"VSETATTR", b"{vg}emb", b"a", b"shard:east"]).await),
        1
    );
    assert_eq!(rb(&a, &[b"VGETATTR", b"{vg}emb", b"a"]).await, "shard:east");
    assert_eq!(
        rt(&a, &[b"VGETATTR", b"{vg}emb", b"b"]).await,
        "$-1",
        "unset attribute is nil"
    );

    // c) Similarity: a probe on a's axis ranks a first; WITHSCORES
    //    pairs member and score. VSIM's VALUES takes the vector with
    //    NO dim argument (the stored width is the arity).
    let near = rf(
        &a,
        &[b"VSIM", b"{vg}emb", b"VALUES", b"1", b"0", b"0", b"0"],
    )
    .await;
    assert_eq!(bulk_at(near.as_bytes(), 0), "a", "nearest first: {near}");
    let scored = rf(
        &a,
        &[
            b"VSIM",
            b"{vg}emb",
            b"WITHSCORES",
            b"VALUES",
            b"1",
            b"0",
            b"0",
            b"0",
        ],
    )
    .await;
    assert_eq!(bulk_at(scored.as_bytes(), 0), "a");
    assert_eq!(
        bulk_at(scored.as_bytes(), 1),
        "1",
        "cosine of an identical vector: {scored}"
    );

    // d) VREM c retires one element: the count drops, VDIM never moves.
    assert_eq!(int_of(&r(&a, &[b"VREM", b"{vg}emb", b"c"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"{vg}emb"]).await), 2);
    assert_eq!(
        int_of(&r(&a, &[b"VDIM", b"{vg}emb"]).await),
        4,
        "width outlives members"
    );
    let after = rf(
        &a,
        &[b"VSIM", b"{vg}emb", b"VALUES", b"0", b"0", b"1", b"0"],
    )
    .await;
    assert!(
        !after.split("\r\n").any(|l| l == "c"),
        "retired member never matches: {after}"
    );

    // e) The ES mirror: PUT the index, _bulk three skus, refresh, then
    //    a range query prices the mid band and a size-capped search
    //    returns exactly one row.
    let mappings = r#"{"mappings":{"properties":{
        "sku":{"type":"keyword"},"price":{"type":"double"}}}}"#;
    let (status, body) = http(&es, "PUT", "/vgidx", Some(mappings)).await;
    assert_eq!(status, 200, "{body}");
    let bulk = concat!(
        r#"{"index":{"_index":"vgidx","_id":"a"}}"#,
        "\n",
        r#"{"sku":"a","price":9.5}"#,
        "\n",
        r#"{"index":{"_index":"vgidx","_id":"b"}}"#,
        "\n",
        r#"{"sku":"b","price":19.5}"#,
        "\n",
        r#"{"index":{"_index":"vgidx","_id":"c"}}"#,
        "\n",
        r#"{"sku":"c","price":29.5}"#,
        "\n",
    );
    let (_status, body) = http(&es, "POST", "/_bulk", Some(bulk)).await;
    assert_eq!(json_of(&body)["errors"], false, "{body}");
    let (status, _) = http(&es, "POST", "/vgidx/_refresh", None).await;
    assert_eq!(status, 200);
    let band = r#"{"query":{"range":{"price":{"gte":10,"lte":25}}}}"#;
    let (_status, body) = http(&es, "POST", "/vgidx/_search", Some(band)).await;
    assert_eq!(json_of(&body)["hits"]["total"]["value"], 1, "{body}");
    assert_eq!(json_of(&body)["hits"]["hits"][0]["_source"]["sku"], "b");
    let capped = r#"{"query":{"range":{"price":{"gte":0,"lte":100}}},"size":1}"#;
    let (_status, body) = http(&es, "POST", "/vgidx/_search", Some(capped)).await;
    assert_eq!(
        json_of(&body)["hits"]["hits"].as_array().map(Vec::len),
        Some(1),
        "size caps the page: {body}"
    );
}
