//! Multi-store scenario #47 -- a crypto micro-exchange desk: the ask
//! book is a ZSET (ZCOUNT sizes the price band, ZMSCORE reads a batch
//! of levels with nils for missing), ZPOPMAX drains the liquidation
//! top, every fill rides the tape stream, MySQL keeps OHLC candles,
//! and ES mirrors the trade tape for the dashboard. One REAL node,
//! RESP + MySQL + ES.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, http, int_of, json_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn ask_book_tape_candles_and_dashboard_agree() {
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, a) = front_world("cryp47", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // a) The ask book: level -> price.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{ex}asks", b"67100", b"L1", b"67150", b"L2", b"67200", b"L3", b"67300", b"L4"]).await),
        4
    );
    assert_eq!(int_of(&r(&a, &[b"ZCOUNT", b"{ex}asks", b"67100", b"67250"]).await), 3);
    // Batch level read: hit, miss, hit (miss renders as a nil bulk).
    let levels = rf(&a, &[b"ZMSCORE", b"{ex}asks", b"L1", b"LX", b"L3"]).await;
    assert!(levels.contains("67100") && levels.contains("67200"), "{levels}");
    assert!(levels.contains("$-1"), "missing level is nil: {levels}");

    // b) A liquidation sweeps the top: ZPOPMAX takes L4 + its price.
    let liq = rf(&a, &[b"ZPOPMAX", b"{ex}asks"]).await;
    assert!(liq.contains("L4") && liq.contains("67300"), "top popped: {liq}");
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"{ex}asks"]).await), 3);

    // c) Fills ride the tape (topic name -> implicit q0 queue).
    for (who, px) in [("taker1", "67100"), ("taker2", "67150"), ("taker3", "67150")] {
        let id = r(&a, &[b"XADD", b"mkt/tape", b"*", b"who", who.as_bytes(), b"px", px.as_bytes()]).await;
        assert!(id.starts_with(b"$") && id.contains(&b'-'), "tape entry {who}: {id:?}");
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"mkt/tape"]).await), 3);

    // d) OHLC candle in MySQL from the three fills.
    ddl(
        &mut c,
        "CREATE TABLE candles (symbol VARCHAR(16) PRIMARY KEY, o DECIMAL(18,2) NOT NULL, \
         h DECIMAL(18,2) NOT NULL, l DECIMAL(18,2) NOT NULL, c DECIMAL(18,2) NOT NULL, vol INT NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO candles (symbol, o, h, l, c, vol) \
                 VALUES ('BTC', 67100, 67150, 67100, 67150, 3)").await;
    assert_eq!(
        ints(&mut c, "SELECT vol FROM candles WHERE symbol = 'BTC'").await,
        vec![3]
    );

    // e) ES mirrors the tape for the dashboard: explicit index first,
    //    then _bulk the three fills.
    let mappings = r#"{"mappings":{"properties":{
        "who":{"type":"keyword"},"px":{"type":"keyword"}}}}"#;
    let (status, body) = http(&es, "PUT", "/tape", Some(mappings)).await;
    assert_eq!(status, 200, "{body}");
    let bulk = concat!(
        r#"{"index":{"_index":"tape","_id":"f1"}}"#, "\n",
        r#"{"who":"taker1","px":"67100"}"#, "\n",
        r#"{"index":{"_index":"tape","_id":"f2"}}"#, "\n",
        r#"{"who":"taker2","px":"67150"}"#, "\n",
        r#"{"index":{"_index":"tape","_id":"f3"}}"#, "\n",
        r#"{"who":"taker3","px":"67150"}"#, "\n",
    );
    let (_status, body) = http(&es, "POST", "/_bulk", Some(bulk)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_of(&body)["errors"], false, "{body}");
    let (_status, body) = http(&es, "POST", "/tape/_count", Some("{}")).await;
    assert_eq!(json_of(&body)["count"], 3, "{body}");
    // The liquidated level never traded.
    let q = r#"{"query":{"term":{"px":"67300"}}}"#;
    let (_status, body) = http(&es, "POST", "/tape/_search", Some(q)).await;
    assert_eq!(json_of(&body)["hits"]["total"]["value"], 0, "{body}");

    // f) The tape on RESP and the book agree post-sweep.
    let ids = scenario_common::stream_ids(&rf(&a, &[b"XRANGE", b"mkt/tape", b"-", b"+" ]).await);
    assert_eq!(ids.len(), 3, "three fills on the tape");
    assert_eq!(rb(&a, &[b"ZSCORE", b"{ex}asks", b"L2"]).await, "67150");
}
