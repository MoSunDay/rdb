//! Multi-store scenario #12 -- a flash sale: the RESP plane carries the
//! hot path -- INCRBY stock countdown, `SET NX PX` per-buyer purchase
//! locks (second lock of the same buyer is a nil reply), a ZINCRBY
//! sales board read back by ZSCORE, the winners strip in a List, and
//! one XADD event per sale -- while the MySQL plane owns the orders
//! with a UNIQUE(order_no) so a replayed order is refused at COMMIT
//! (1062-at-commit, order_pipeline semantics) without touching stock.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, int_of, ok, r, rb, rf, FrontSpec};

#[tokio::test]
async fn flash_sale_locks_stock_orders_and_winners() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: RESP + MySQL front; the orders table dedupes by no.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("flash12", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, order_no VARCHAR(40) NOT NULL, \
         buyer VARCHAR(24) NOT NULL, qty INT NOT NULL, \
         UNIQUE KEY uk_no (order_no))",
    )
    .await;

    // b) Stock: 50 units loaded, three single-unit sales drain it to 47.
    assert_eq!(
        int_of(&r(&a, &[b"INCRBY", b"flash:stock", b"50"]).await),
        50
    );
    for want in [49, 48, 47] {
        assert_eq!(
            int_of(&r(&a, &[b"INCRBY", b"flash:stock", b"-1"]).await),
            want,
            "stock drains one per sale"
        );
    }

    // c) Per-buyer purchase lock: buyer u1 takes it, the same buyer's
    //    second attempt gets a nil bulk (SET NX refusal).
    ok(
        &r(
            &a,
            &[b"SET", b"flash:lock:u1", b"held", b"NX", b"PX", b"30000"],
        )
        .await,
        "first lock taken",
    );
    let refused = r(
        &a,
        &[b"SET", b"flash:lock:u1", b"again", b"NX", b"PX", b"30000"],
    )
    .await;
    assert!(refused.starts_with(b"$-1"), "re-lock refused: {refused:?}");
    ok(
        &r(
            &a,
            &[b"SET", b"flash:lock:u2", b"held", b"NX", b"PX", b"30000"],
        )
        .await,
        "a different buyer still can",
    );

    // d) Three orders land in SQL; the replay of SO-9001 stages fine
    //    but COMMIT refuses the duplicate order_no, whole-txn rollback.
    for (i, no, buyer) in [
        (1, "SO-9001", "u1"),
        (2, "SO-9002", "u2"),
        (3, "SO-9003", "u3"),
    ] {
        run(
            &mut c,
            &format!(
                "INSERT INTO orders (id, order_no, buyer, qty) VALUES ({i}, '{no}', '{buyer}', 1)"
            ),
        )
        .await;
    }
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "INSERT INTO orders (id, order_no, buyer, qty) VALUES (9, 'SO-9001', 'u1', 1)",
    )
    .await;
    let err = c
        .query_drop("COMMIT")
        .await
        .expect_err("duplicate order_no must be refused at COMMIT");
    let msg = err.to_string();
    assert!(
        msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"),
        "dup refusal: {msg}"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM orders").await,
        vec![3],
        "the refused replay left exactly the three orders"
    );

    // e) Sales board: u1 bought 2, u2 bought 1; ZSCORE reads them back.
    assert_eq!(
        bulk_first(&r(&a, &[b"ZINCRBY", b"flash:sales", b"1", b"u1"]).await),
        "1"
    );
    assert_eq!(
        bulk_first(&r(&a, &[b"ZINCRBY", b"flash:sales", b"1", b"u1"]).await),
        "2"
    );
    assert_eq!(
        bulk_first(&r(&a, &[b"ZINCRBY", b"flash:sales", b"1", b"u2"]).await),
        "1"
    );
    assert_eq!(rb(&a, &[b"ZSCORE", b"flash:sales", b"u1"]).await, "2");
    assert_eq!(rb(&a, &[b"ZSCORE", b"flash:sales", b"u2"]).await, "1");
    assert_eq!(
        rb(&a, &[b"ZSCORE", b"flash:sales", b"u3"]).await,
        "",
        "u3 never bought"
    );

    // f) Winners strip: three winners pushed, LRANGE sees them all.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"LPUSH",
                    b"flash:winners",
                    b"SO-9003",
                    b"SO-9002",
                    b"SO-9001"
                ]
            )
            .await
        ),
        3
    );
    let strip = rf(&a, &[b"LRANGE", b"flash:winners", b"0", b"-1"]).await;
    for no in ["SO-9001", "SO-9002", "SO-9003"] {
        assert!(strip.contains(no), "{no} on the strip: {strip}");
    }
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"flash:winners"]).await), 3);

    // g) One event per sale rides the stream; XLEN counts three.
    for no in ["SO-9001", "SO-9002", "SO-9003"] {
        let event = r(
            &a,
            &[
                b"XADD",
                b"flash/events",
                b"*",
                b"order",
                no.as_bytes(),
                b"state",
                b"paid",
            ],
        )
        .await;
        assert!(
            scenario_common::bulk_at(&event, 0).contains('-'),
            "event {no}"
        );
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"flash/events"]).await), 3);

    // h) Reconciliation: stock left (47) + sold (3) == 50 loaded, and
    //    SQL's sold count matches the board's total.
    let stock = rb(&a, &[b"GET", b"flash:stock"])
        .await
        .parse::<i64>()
        .unwrap();
    let sold_sql = ints(&mut c, "SELECT SUM(qty) FROM orders").await;
    assert_eq!(sold_sql, vec![3]);
    assert_eq!(stock + sold_sql[0], 50, "no stock leaked");
    let mut board_total = 0i64;
    for m in ["u1", "u2"] {
        board_total += rb(&a, &[b"ZSCORE", b"flash:sales", m.as_bytes()])
            .await
            .parse::<i64>()
            .unwrap();
    }
    assert_eq!(board_total, 3, "board == SQL sold");
}

/// First bulk payload of a ZINCRBY reply (the new score).
fn bulk_first(reply: &[u8]) -> String {
    scenario_common::bulk_at(reply, 0)
}
