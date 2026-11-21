//! Multi-store scenario #45 -- a warehouse pick-wave planner: each
//! lane keeps a ZSET of SKU priorities, ZINTERSTORE merges two lanes
//! into the combined wave (scores sum), ZDIFFSTORE isolates SKUs only
//! lane 1 stocks, SMISMEMBER batch-checks the fragile set, and MySQL
//! holds the pick lines both RESP reads reconcile against. One REAL
//! node, RESP + MySQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rb, FrontSpec};

#[tokio::test]
async fn pick_wave_merges_lanes_and_isolates_exclusives() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("whse45", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Two stock lanes as priority ZSETs; sku2/sku3 live in both.
    assert_eq!(int_of(&r(&a, &[b"ZADD", b"{wh}lane1", b"1", b"sku1", b"2", b"sku2", b"3", b"sku3"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"ZADD", b"{wh}lane2", b"5", b"sku2", b"4", b"sku3", b"7", b"sku4"]).await), 3);

    // b) The combined wave: ZINTERSTORE sums the member scores.
    assert_eq!(int_of(&r(&a, &[b"ZINTERSTORE", b"{wh}wave", b"2", b"{wh}lane1", b"{wh}lane2"]).await), 2);
    assert_eq!(rb(&a, &[b"ZSCORE", b"{wh}wave", b"sku2"]).await, "7", "2+5");
    assert_eq!(rb(&a, &[b"ZSCORE", b"{wh}wave", b"sku3"]).await, "7", "3+4");
    // WEIGHTS tilt the merge: lane1 x3.
    assert_eq!(
        int_of(&r(&a, &[b"ZINTERSTORE", b"{wh}tilt", b"2", b"{wh}lane1", b"{wh}lane2", b"WEIGHTS", b"3", b"1"]).await),
        2
    );
    assert_eq!(rb(&a, &[b"ZSCORE", b"{wh}tilt", b"sku2"]).await, "11", "2*3+5");

    // c) Lane-1 exclusives: ZDIFFSTORE keeps only lane1's members.
    assert_eq!(
        int_of(&r(&a, &[b"ZDIFFSTORE", b"{wh}only1", b"2", b"{wh}lane1", b"{wh}lane2"]).await),
        1
    );
    assert_eq!(rb(&a, &[b"ZSCORE", b"{wh}only1", b"sku1"]).await, "1");
    assert_eq!(rb(&a, &[b"ZSCORE", b"{wh}only1", b"sku2"]).await, "", "diffed out");

    // d) Fragile manifest: batch membership check.
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{wh}fragile", b"sku1", b"sku3"]).await), 2);
    let flags = scenario_common::rf(&a, &[b"SMISMEMBER", b"{wh}fragile", b"sku1", b"sku4", b"sku3"]).await;
    assert!(flags.contains(":1") && flags.contains(":0"), "mixed flags: {flags}");
    let ones = flags.matches(":1").count();
    assert_eq!(ones, 2, "sku1 and sku3 fragile: {flags}");

    // e) Pick lines in MySQL mirror the merged wave.
    ddl(
        &mut c,
        "CREATE TABLE picks (id BIGINT PRIMARY KEY, sku VARCHAR(16) NOT NULL, \
         lane VARCHAR(8) NOT NULL, qty INT NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO picks (id, sku, lane, qty) VALUES \
                 (1, 'sku2', 'both', 7), (2, 'sku3', 'both', 7), (3, 'sku1', 'lane1', 1)").await;
    assert_eq!(ints(&mut c, "SELECT SUM(qty) FROM picks WHERE lane = 'both'").await, vec![14]);
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM picks WHERE sku NOT IN ('sku4')").await,
        vec![3],
        "sku4 never picked (lane2 exclusive)"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"{wh}wave"]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"{wh}only1"]).await), 1);
}
