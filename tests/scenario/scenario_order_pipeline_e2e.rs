//! Multi-store scenario #1 -- e-commerce order pipeline, one story over
//! FOUR storage classes in a single REAL node: the shopping cart lives
//! in a Hash, checkout runs as a SQL transaction with a UNIQUE order_no
//! as the idempotency guard (semantics: sql_txn_e2e BEGIN/COMMIT +
//! sql_index_e2e UNIQUE/1062), fulfillment events ride a lite stream
//! consumed by XREADGROUP/XACK (lite_group_e2e), and the sales board is
//! a ZSET reconciled against a SQL SUM at the end. Single bootstrap
//! node, no CLUSTER INIT (no MOVED noise; kafka_sdk/process precedent).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, one, run};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{bulk_at, front_world, int_of, ok, r, rf, FrontSpec};

#[tokio::test]
async fn order_pipeline_cart_to_reconciliation() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: RESP + the MySQL front. Seed the catalog: three SKUs
    //    with on-hand stock.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("order1", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE stock (item_id BIGINT PRIMARY KEY, sku VARCHAR(32), qty INT NOT NULL)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, order_no VARCHAR(40) NOT NULL, \
         item_id BIGINT NOT NULL, qty INT NOT NULL, amount DECIMAL(10,2) NOT NULL, \
         UNIQUE KEY uk_order (order_no))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO stock (item_id, sku, qty) VALUES \
         (1, 'widget', 100), (2, 'gadget', 50), (3, 'gizmo', 7)",
    )
    .await;

    // b) The cart: two lines in a Hash, read back field-wise.
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"cart:u1", b"sku:1", b"2", b"sku:2", b"1"]).await),
        2,
        "two cart lines"
    );
    let cart = rf(&a, &[b"HGETALL", b"cart:u1"]).await;
    assert!(cart.contains("sku:1") && cart.contains("sku:2"), "{cart}");

    // c) Checkout = one SQL transaction: reserve stock + insert the
    //    order guarded by the UNIQUE order_no.
    for (oid, (no, item, qty, amount)) in [
        (1i64, ("SO-1001", 1i64, 2i64, "29.90")),
        (2, ("SO-1002", 2, 1, "19.00")),
    ]
    .into_iter()
    {
        run(&mut c, "BEGIN").await;
        run(
            &mut c,
            &format!("UPDATE stock SET qty = qty - {qty} WHERE item_id = {item} AND qty >= {qty}"),
        )
        .await;
        run(
            &mut c,
            &format!(
                "INSERT INTO orders (id, order_no, item_id, qty, amount) \
                      VALUES ({oid}, '{no}', {item}, {qty}, {amount})"
            ),
        )
        .await;
        run(&mut c, "COMMIT").await;
    }
    let left = ints(
        &mut c,
        "SELECT qty FROM stock WHERE item_id IN (1,2) ORDER BY item_id",
    )
    .await;
    assert_eq!(left, vec![98, 49], "stock decremented inside the txn");

    // d) The retry of a duplicate order_no stages fine but the COMMIT
    //    is refused (snapshot txns validate write conflicts at commit;
    //    the dup order_no trips the UNIQUE index there) and NOTHING it
    //    staged -- including the stock decrement -- lands.
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "UPDATE stock SET qty = qty - 2 WHERE item_id = 1 AND qty >= 2",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO orders (id, order_no, item_id, qty, amount) \
         VALUES (2, 'SO-1001', 1, 2, '29.90')",
    )
    .await;
    let refused = c
        .query_drop("COMMIT")
        .await
        .expect_err("duplicate order_no commit must fail");
    let msg = refused.to_string();
    assert!(
        msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"),
        "expected a dup/conflict refusal, got: {msg}"
    );
    let still = ints(&mut c, "SELECT qty FROM stock WHERE item_id = 1").await;
    assert_eq!(still, vec![98], "the refused retry left stock alone");

    // e) Fulfillment: both orders publish events onto one stream, the
    //    packing group consumes them exactly once and acks.
    let mut event_ids: Vec<Vec<u8>> = Vec::new();
    for no in ["SO-1001", "SO-1002"] {
        let reply = r(
            &a,
            &[
                b"XADD",
                b"orders/q0",
                b"*",
                b"order",
                no.as_bytes(),
                b"status",
                b"paid",
            ],
        )
        .await;
        let id = bulk_at(&reply, 0);
        assert!(id.contains('-'), "xadd minted id {id:?}");
        event_ids.push(id.into_bytes());
    }
    ok(
        &r(
            &a,
            &[b"XGROUP", b"CREATE", b"orders/q0", b"packers", b"0-0"],
        )
        .await,
        "xgroup create",
    );
    let batch = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"packers",
            b"c1",
            b"COUNT",
            b"10",
            b"STREAMS",
            b"orders/q0",
            b">",
        ],
    )
    .await;
    assert!(
        batch.contains("SO-1001") && batch.contains("SO-1002"),
        "{batch}"
    );
    let mut ack_args: Vec<&[u8]> = vec![b"XACK", b"orders/q0", b"packers"];
    event_ids.iter().for_each(|id| ack_args.push(id.as_slice()));
    assert_eq!(
        int_of(&r(&a, &ack_args).await),
        2,
        "both events acked by their minted ids"
    );
    let again = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"packers",
            b"c1",
            b"COUNT",
            b"10",
            b"STREAMS",
            b"orders/q0",
            b">",
        ],
    )
    .await;
    assert!(
        !again.contains("SO-"),
        "no redelivery before ack loss: {again}"
    );

    // f) The sales board accumulates per-SKU sold quantities in a ZSET.
    for (delta, sku, want) in [("2", "widget", "2"), ("1", "gadget", "1")] {
        let reply = r(
            &a,
            &[b"ZINCRBY", b"board:sku", delta.as_bytes(), sku.as_bytes()],
        )
        .await;
        assert_eq!(bulk_at(&reply, 0), want, "{sku} -> {want}");
    }
    let board = rf(&a, &[b"ZRANGE", b"board:sku", b"0", b"-1", b"WITHSCORES"]).await;
    assert!(
        board.contains("widget") && board.contains("gadget"),
        "{board}"
    );

    // g) Reconciliation: the SQL ledger's SUM(qty) equals the board's
    //    ZSCORE totals -- two storage classes, one truth.
    let sql_sum = one(&mut c, "SELECT SUM(qty) FROM orders").await;
    let sql_sum = match sql_sum {
        MVal::Int(n) => n,
        MVal::Bytes(b) => String::from_utf8_lossy(&b).parse().expect("sum digits"),
        other => panic!("SUM cell {other:?}"),
    };
    let mut board_sum = 0i64;
    for sku in ["widget", "gadget"] {
        let score = bulk_at(&r(&a, &[b"ZSCORE", b"board:sku", sku.as_bytes()]).await, 0);
        board_sum += score.parse::<i64>().expect("zscore digits");
    }
    assert_eq!(
        sql_sum, board_sum,
        "SQL SUM(qty) must equal the ZSET board total"
    );
    assert_eq!(sql_sum, 3, "2 widgets + 1 gadget");
    // The cart survives too -- checkout never deleted it.
    assert_eq!(int_of(&r(&a, &[b"HLEN", b"cart:u1"]).await), 2);
}
