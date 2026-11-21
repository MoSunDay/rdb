//! Multi-store scenario #55 -- product search: an FT index over
//! title/brand serves exact-term queries (FT.SEARCH hits one doc by
//! title, two by brand), the teardown is pinned (FT.DROP takes the
//! whole family and answers :1, later searches fail with unknown
//! index; FT.DROPINDEX is FT.DROP's alias -- documented deviation:
//! no doc-preserving drop), a popularity ZSET feeds autocomplete, and
//! a MySQL catalog answers the same prefix via LIKE. One REAL node.

mod common;
mod scenario_common;

use common::mysql::{col, connect_root, ddl, run};
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn catalog_searches_drops_and_autocompletes() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("ps55", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The index: title + brand, three docs land.
    scenario_common::ok(
        &r(&a, &[b"FT.CREATE", b"{ps}idx", b"SCHEMA", b"title", b"TEXT", b"brand", b"TEXT"]).await,
        "create index",
    );
    for (id, doc) in [
        ("p1", r#"{"title":"cordless drill","brand":"acme"}"#),
        ("p2", r#"{"title":"drill bit set","brand":"acme"}"#),
        ("p3", r#"{"title":"desk lamp","brand":"nova"}"#),
    ] {
        assert_eq!(
            int_of(&r(&a, &[b"FT.ADD", b"{ps}idx", id.as_bytes(), doc.as_bytes()]).await),
            1,
            "add {id}"
        );
    }

    // b) Exact-term search: title hits one, brand hits two, and a
    //    missing index answers a plain error.
    let drill = rf(&a, &[b"FT.SEARCH", b"{ps}idx", b"@title:drill"]).await;
    assert!(drill.contains("p1") && drill.contains("p2") && !drill.contains("p3"), "{drill}");
    let acme = rf(&a, &[b"FT.SEARCH", b"{ps}idx", b"@brand:acme"]).await;
    assert!(acme.contains("p1") && acme.contains("p2") && !acme.contains("p3"), "{acme}");
    assert_eq!(rt(&a, &[b"FT.SEARCH", b"{ps}gone", b"@title:x"]).await, "-ERR unknown index");

    // c) Teardown: FT.DROP takes the family (:1), the searches fail,
    //    and FT.DROPINDEX is the same verb on a second index.
    assert_eq!(int_of(&r(&a, &[b"FT.DROP", b"{ps}idx"]).await), 1);
    assert_eq!(rt(&a, &[b"FT.SEARCH", b"{ps}idx", b"@title:drill"]).await, "-ERR unknown index");
    scenario_common::ok(&r(&a, &[b"FT.CREATE", b"{ps}alt", b"SCHEMA", b"t", b"TEXT"]).await, "alt index");
    assert_eq!(
        int_of(&r(&a, &[b"FT.ADD", b"{ps}alt", b"x1", b"{\"t\":\"warm\"}"]).await),
        1,
        "alt doc"
    );
    assert_eq!(int_of(&r(&a, &[b"FT.DROPINDEX", b"{ps}alt"]).await), 1);
    assert_eq!(rt(&a, &[b"FT.DROPINDEX", b"{ps}alt"]).await, "-ERR unknown index");

    // d) Autocomplete: popularity ZSET ranks the typed prefix's
    //    suggestions (score order, byte order inside a tie).
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{ps}sugg", b"3", b"drill", b"2", b"driver", b"5", b"desk"]).await),
        3
    );
    let sugg = rf(&a, &[b"ZRANGE", b"{ps}sugg", b"0", b"-1"]).await;
    assert_eq!(bulk_at(sugg.as_bytes(), 0), "driver", "{sugg}");
    assert_eq!(bulk_at(sugg.as_bytes(), 1), "drill", "{sugg}");
    assert_eq!(bulk_at(sugg.as_bytes(), 2), "desk", "{sugg}");
    assert_eq!(rb(&a, &[b"ZSCORE", b"{ps}sugg", b"desk"]).await, "5");

    // e) The MySQL catalog answers the same prefix via LIKE.
    ddl(
        &mut c,
        "CREATE TABLE catalog (id BIGINT PRIMARY KEY, title VARCHAR(64) NOT NULL, brand VARCHAR(32) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO catalog (id, title, brand) VALUES \
        (1,'cordless drill','acme'), (2,'drill bit set','acme'), (3,'desk lamp','nova')")
        .await;
    let titles = col(&mut c, "SELECT title FROM catalog WHERE title LIKE 'cor%'").await;
    assert_eq!(titles.len(), 1, "one cordless hit");
    run(&mut c, "DELETE FROM catalog WHERE id = 3").await;
    assert_eq!(col(&mut c, "SELECT title FROM catalog WHERE brand = 'acme'").await.len(), 2);
}
