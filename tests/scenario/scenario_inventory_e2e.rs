//! Multi-store scenario #16 -- a warehouse inventory: per-SKU state is
//! a Hash with HINCRBY quantity moves, warehouse membership is a Set
//! (SADD dedup :0, SISMEMBER, SMOVE between warehouses), the restock
//! queue is a ZSET of gaps swept by ZREMRANGEBYSCORE once the crew
//! restocks, every move is journaled to a stream (XADD) and reconciled
//! in SQL (SUM(deltas) == HGET qty), and the restock command itself
//! arrives through the RocksMQ HTTP front (produce/consume/ack,
//! pending_count drains to zero -- log_pipeline MQ shapes).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{codec::b64d, front_world, int_of, mq_post, r, rb, rf, FrontSpec};

#[tokio::test]
async fn inventory_moves_restock_queue_and_mq_commands() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: MySQL + RocksMQ HTTP fronts.
    let spec = FrontSpec {
        mysql: true,
        rocksmq: true,
        ..Default::default()
    };
    let (front, a) = front_world("inv16", &spec).await;
    let mq = front.rocksmq.clone();
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE movements (id BIGINT PRIMARY KEY, sku VARCHAR(8) NOT NULL, delta INT NOT NULL)").await;

    // b) Per-SKU Hash: seed two SKUs, drain them with HINCRBY moves.
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"inv:sku1", b"qty", b"10", b"bin", b"A1"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"inv:sku2", b"qty", b"8", b"bin", b"B2"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"inv:sku1", b"qty", b"-7"]).await),
        3,
        "sku1 -> 3"
    );
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"inv:sku2", b"qty", b"-4"]).await),
        4,
        "sku2 -> 4"
    );

    // c) Warehouse membership: SADD dedupes (:0 on repeat), SISMEMBER
    //    answers, SMOVE transfers between warehouses.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{inv}wh:west", b"sku1", b"sku2", b"sku3"]).await),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{inv}wh:west", b"sku1"]).await),
        0,
        "already a member"
    );
    assert_eq!(
        int_of(&r(&a, &[b"SISMEMBER", b"{inv}wh:west", b"sku2"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"SMOVE", b"{inv}wh:west", b"{inv}wh:east", b"sku3"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"SCARD", b"{inv}wh:west"]).await),
        2,
        "sku3 left west"
    );
    let east = rf(&a, &[b"SMEMBERS", b"{inv}wh:east"]).await;
    assert!(east.contains("sku3"), "{east}");

    // d) The restock queue: ZSET keyed by gap (worst gaps first when
    //    ascending). After the crew restocks both SKUs the sweep drains
    //    every open ticket with ZREMRANGEBYSCORE.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"inv:low", b"7", b"sku1", b"4", b"sku2"]).await),
        2
    );
    let low = rf(&a, &[b"ZRANGE", b"inv:low", b"0", b"-1", b"WITHSCORES"]).await;
    assert!(
        low.find("sku2").unwrap() < low.find("sku1").unwrap(),
        "gap 4 before gap 7: {low}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"inv:sku1", b"qty", b"7"]).await),
        10,
        "restocked"
    );
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"inv:sku2", b"qty", b"4"]).await),
        8,
        "restocked"
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZREMRANGEBYSCORE", b"inv:low", b"(0", b"+inf"]).await),
        2,
        "both tickets drained"
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZCARD", b"inv:low"]).await),
        0,
        "queue empty"
    );

    // e) Every move is journaled onto the stream: the initial receipt
    //    of stock counts as a move too, so the ledger can close.
    let moves = [
        ("sku1", 10),
        ("sku1", -7),
        ("sku1", 7),
        ("sku2", 8),
        ("sku2", -4),
        ("sku2", 4),
    ];
    for (sku, d) in moves {
        let ev = r(
            &a,
            &[
                b"XADD",
                b"inv/moves",
                b"*",
                b"sku",
                sku.as_bytes(),
                b"delta",
                d.to_string().as_bytes(),
            ],
        )
        .await;
        assert!(
            scenario_common::bulk_at(&ev, 0).contains('-'),
            "move {sku} {d}"
        );
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"inv/moves"]).await), 6);

    // f) SQL reconciliation: the movement ledger sums back to the Hash.
    for (i, (sku, d)) in moves.iter().enumerate() {
        run(
            &mut c,
            &format!(
                "INSERT INTO movements (id, sku, delta) VALUES ({}, '{sku}', {d})",
                i + 1
            ),
        )
        .await;
    }
    let s1 = ints(
        &mut c,
        "SELECT SUM(delta) FROM movements WHERE sku = 'sku1'",
    )
    .await;
    assert_eq!(s1, vec![10], "sku1 ledger sums to 10");
    assert_eq!(
        rb(&a, &[b"HGET", b"inv:sku1", b"qty"]).await,
        "10",
        "Hash agrees"
    );
    let s2 = ints(
        &mut c,
        "SELECT SUM(delta) FROM movements WHERE sku = 'sku2'",
    )
    .await;
    assert_eq!(s2, vec![8]);
    assert_eq!(rb(&a, &[b"HGET", b"inv:sku2", b"qty"]).await, "8");

    // g) The restock command rides RocksMQ HTTP: produce, consume
    //    (base64 body decoded), ack, pending drained to zero.
    let cmd = "restock sku1 +7";
    let (status, body) = mq_post(&mq, "/produce?channel=restock", cmd).await;
    assert_eq!(status, 200, "produce: {body}");
    let (status, body) = mq_post(&mq, "/consume?channel=restock&group=crew&n=10", "").await;
    assert_eq!(status, 200, "consume: {body}");
    let items = common::mq::msgs_of(&body);
    assert_eq!(items.len(), 1, "one command: {body}");
    let (id, b64) = &items[0];
    assert_eq!(b64d(b64), cmd, "decoded command matches");
    let (status, body) =
        mq_post(&mq, &format!("/ack?channel=restock&group=crew&id={id}"), "").await;
    assert_eq!(status, 200, "ack: {body}");
    assert_eq!(
        common::mq::pending_count(&mq, "restock", "crew").await,
        0,
        "queue drained"
    );
    // The HTTP channel is the stream restock/q0 on the RESP plane.
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"restock/q0"]).await),
        1,
        "mq channel == stream"
    );
}
