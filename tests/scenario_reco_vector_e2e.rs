//! Multi-store scenario #4 -- personalized recommendations: a SQL
//! product catalog (the ground truth), a VectorSet of item embeddings
//! for similarity recall (vectorset_e2e VADD/VSIM shapes), a Hash per
//! item for metadata reads, and a ZSET of live hotness. The fusion
//! assertion: all THREE signals agree on the same top item.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, one, run, rows, s};
use mysql_async::Value as MVal;
use scenario_common::{bulk_at, front_world, int_of, r, rf, FrontSpec};

#[tokio::test]
async fn recommendation_recall_fusion_across_three_signals() {
    // a) One node: RESP + MySQL front. The catalog is SQL.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("reco4", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR(64) NOT NULL, \
         cat VARCHAR(32) NOT NULL, price DECIMAL(10,2) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO items (id, name, cat, price) VALUES \
         (1, 'drill', 'tools', 89.00), (2, 'scarf', 'apparel', 19.50), \
         (3, 'toolbelt', 'tools', 29.00)")
    .await;

    // b) A 2-D embedding per item (hand-placed so the geometry is
    //    exact): drill=[1,0], scarf=[0,1], toolbelt=[1,1].
    assert_eq!(int_of(&r(&a, &[b"VADD", b"reco:emb", b"VALUES", b"2", b"1", b"1", b"0"]).await), 1, "vadd drill");
    assert_eq!(int_of(&r(&a, &[b"VADD", b"reco:emb", b"VALUES", b"2", b"2", b"0", b"1"]).await), 1, "vadd scarf");
    assert_eq!(int_of(&r(&a, &[b"VADD", b"reco:emb", b"VALUES", b"2", b"3", b"1", b"1"]).await), 1, "vadd belt");
    assert_eq!(int_of(&r(&a, &[b"VCARD", b"reco:emb"]).await), 3);

    // c) Live hotness: the shopper browsed tools; drill is the hottest.
    assert_eq!(bulk_at(&r(&a, &[b"ZINCRBY", b"reco:hot", b"10", b"1"]).await, 0), "10", "hot drill");
    assert_eq!(bulk_at(&r(&a, &[b"ZINCRBY", b"reco:hot", b"4", b"2"]).await, 0), "4", "hot scarf");
    assert_eq!(bulk_at(&r(&a, &[b"ZINCRBY", b"reco:hot", b"7", b"3"]).await, 0), "7", "hot belt");

    // d) Recall: the query vector [1,0] (this shopper's taste) ranks
    //    drill (score 1) first, toolbelt (diagonal) second, scarf last.
    let recall = rf(
        &a,
        &[b"VSIM", b"reco:emb", b"COUNT", b"3", b"VALUES", b"1", b"0"],
    )
    .await;
    assert_eq!(bulk_at(recall.as_bytes(), 0), "1", "drill nearest: {recall:?}");
    assert_eq!(bulk_at(recall.as_bytes(), 1), "3", "belt second: {recall:?}");
    assert_eq!(bulk_at(recall.as_bytes(), 2), "2", "scarf last: {recall:?}");

    // e) Enrich: metadata for the recalled top item comes from the Hash
    //    cache, and must agree with the SQL catalog row.
    assert_eq!(
        int_of(
            &r(&a, &[b"HSET", b"reco:meta:1", b"name", b"drill", b"cat", b"tools", b"price", b"89.00"]).await
        ),
        3,
        "meta drill"
    );
    let meta = rf(&a, &[b"HGETALL", b"reco:meta:1"]).await;
    assert!(meta.contains("drill") && meta.contains("tools"), "{meta}");
    let cat = rows(
        &mut c,
        "SELECT name, cat, price FROM items WHERE id = 1",
    )
    .await;
    assert_eq!(
        cat,
        vec![vec![s("drill"), s("tools"), s("89.00")]],
        "SQL row matches the Hash cache"
    );

    // f) Fusion verdict: vector top-1 == hotness top-1 == the item the
    //    catalog names. Three storage classes, one recommendation.
    let hot_top = rf(&a, &[b"ZREVRANGE", b"reco:hot", b"0", b"0"]).await;
    assert!(hot_top.contains("1"), "drill tops the hot board: {hot_top}");
    let top_name = one(&mut c, "SELECT name FROM items WHERE id = 1").await;
    assert!(
        matches!(&top_name, MVal::Bytes(b) if b == b"drill"),
        "catalog top name: {top_name:?}"
    );
    // Recall order also matches the catalog's tool-category ordering.
    let tool_ids = ints(&mut c, "SELECT id FROM items WHERE cat = 'tools' ORDER BY id").await;
    assert_eq!(tool_ids, vec![1, 3], "the two recalled tools");
}
