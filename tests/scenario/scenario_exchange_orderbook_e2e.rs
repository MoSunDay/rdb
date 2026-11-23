//! Multi-store scenario #23 -- an exchange order book: resting orders
//! live in two ZSETs keyed by price in integer cents (bids read best
//! first via ZREVRANGE, asks best first via ZRANGE), an aggressive
//! order that crosses the opposite book trades -- both resting leaves
//! leave via ZREM, the fill lands in SQL, the ticker is an INCR/HINCRBY
//! pair, and the match prints to the trade tape (XADD). The no-cross
//! book must stay put: count conserved, ticker agrees with SQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, one, run};
use mysql_async::Value as MVal;
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn exchange_orderbook_match_ticker_and_tape() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: RESP + MySQL front; the trades table is the tape's
    //    durable shadow.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("book23", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE trades (id BIGINT PRIMARY KEY, buy_order VARCHAR(8) NOT NULL, \
         sell_order VARCHAR(8) NOT NULL, price INT NOT NULL, qty INT NOT NULL)",
    )
    .await;

    // b) Rest the books: three bids (10150/10050/9900 cents), two asks
    //    (10200/10350). Best bid b1, best ask a1, spread 50 -- no cross.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"ZADD",
                    b"book:bid",
                    b"10150",
                    b"b1",
                    b"10050",
                    b"b2",
                    b"9900",
                    b"b3"
                ]
            )
            .await
        ),
        3
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"ZADD", b"book:ask", b"10200", b"a1", b"10350", b"a2"]
            )
            .await
        ),
        2
    );
    let best_bid = rf(&a, &[b"ZREVRANGE", b"book:bid", b"0", b"0", b"WITHSCORES"]).await;
    assert!(
        best_bid.contains("b1") && best_bid.contains("10150"),
        "{best_bid}"
    );
    let best_ask = rf(&a, &[b"ZRANGE", b"book:ask", b"0", b"0", b"WITHSCORES"]).await;
    assert!(
        best_ask.contains("a1") && best_ask.contains("10200"),
        "{best_ask}"
    );
    assert_eq!(
        10200 - 10150,
        50,
        "spread (derived from the two best quotes)"
    );

    // c) Aggressive buy b4 at 10250 crosses a1's 10200: both leaves
    //    leave the books (ZREM), the fill trades at the resting price.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"book:bid", b"10250", b"b4"]).await),
        1,
        "b4 rests"
    );
    let crossed = rf(&a, &[b"ZREVRANGE", b"book:bid", b"0", b"0", b"WITHSCORES"]).await;
    assert!(crossed.contains("b4"), "b4 now tops the bid: {crossed}");
    assert_eq!(
        int_of(&r(&a, &[b"ZREM", b"book:bid", b"b4"]).await),
        1,
        "taker leaf removed"
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZREM", b"book:ask", b"a1"]).await),
        1,
        "maker leaf removed"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"book:bid"]).await), 3);
    assert_eq!(
        int_of(&r(&a, &[b"ZCARD", b"book:ask"]).await),
        1,
        "only a2 still resting"
    );

    // d) The fill: SQL row + ticker (trades via HINCRBY, volume via
    //    INCRBY) + one tape event.
    run(&mut c, "INSERT INTO trades (id, buy_order, sell_order, price, qty) VALUES (1, 'b4', 'a1', 10200, 100)").await;
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"mkt:stats", b"trades", b"1"]).await),
        1,
        "first trade"
    );
    assert_eq!(
        int_of(&r(&a, &[b"INCRBY", b"mkt:vol", b"100"]).await),
        100,
        "volume +100"
    );
    let tape = r(
        &a,
        &[
            b"XADD",
            b"tape/trades",
            b"*",
            b"buy",
            b"b4",
            b"sell",
            b"a1",
            b"price",
            b"10200",
            b"qty",
            b"100",
        ],
    )
    .await;
    assert!(bulk_at(&tape, 0).contains('-'), "tape event minted");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"tape/trades"]).await), 1);

    // e) The book after the match: best bid b1 10150 vs best ask a2
    //    10350 -- a resting limit order at 10200 cannot cross it.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"book:bid", b"10200", b"b5"]).await),
        1,
        "b5 rests"
    );
    let no_cross = rf(&a, &[b"ZREVRANGE", b"book:bid", b"0", b"0", b"WITHSCORES"]).await;
    assert!(no_cross.contains("b5"), "{no_cross}");
    let ask_now = rf(&a, &[b"ZRANGE", b"book:ask", b"0", b"0", b"WITHSCORES"]).await;
    assert!(
        ask_now.contains("a2") && ask_now.contains("10350"),
        "{ask_now}"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM trades").await,
        vec![1],
        "no phantom trade"
    );

    // f) Reconciliation: ticker == SQL == tape, one plane each.
    assert_eq!(
        rb(&a, &[b"HGET", b"mkt:stats", b"trades"]).await,
        "1",
        "ticker trades"
    );
    let n = |v: &MVal| match v {
        MVal::Int(i) => *i,
        MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("cell {other:?}"),
    };
    let sql_vol = n(&one(&mut c, "SELECT SUM(qty) FROM trades").await);
    assert_eq!(
        rb(&a, &[b"GET", b"mkt:vol"]).await.parse::<i64>().unwrap(),
        sql_vol,
        "volume == SUM(qty)"
    );
    assert_eq!(sql_vol, 100);
    assert_eq!(
        ints(&mut c, "SELECT price FROM trades WHERE id = 1").await,
        vec![10200],
        "fill price is the resting maker's, not the taker's limit"
    );
}
