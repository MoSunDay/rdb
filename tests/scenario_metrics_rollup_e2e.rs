//! Multi-store scenario #15 -- a metrics pipeline: hit counters are
//! INCR buckets (risk_control fixed-window shape), unique visitors are
//! a SETBIT/GETBIT/BITCOUNT bloom-style bitmap where re-setting a bit
//! answers :1 and never inflates UV, the hot-endpoint board is a ZSET
//! (ZINCRBY + ZREVRANGE ordering), raw samples ride a stream replayed
//! by XRANGE, and the SQL rollup table must reconcile with every RESP
//! plane total.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, one, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, rf, stream_ids, FrontSpec};

#[tokio::test]
async fn metrics_rollup_counters_bitmap_board_and_replay() {
    // a) One node: RESP + MySQL front.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("mtr15", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE rollup (metric VARCHAR(24) PRIMARY KEY, val INT NOT NULL)").await;

    // b) Hit buckets: api takes 3 hits, db takes 1.
    for want in [1, 2, 3] {
        assert_eq!(int_of(&r(&a, &[b"INCR", b"m:hits:api"]).await), want, "api hit {want}");
    }
    assert_eq!(int_of(&r(&a, &[b"INCR", b"m:hits:db"]).await), 1, "db hit 1");

    // c) UV bitmap: first SETBIT answers :0, re-setting the same bit
    //    answers :1 without inflating the count.
    assert_eq!(int_of(&r(&a, &[b"SETBIT", b"m:uv:d1", b"100", b"1"]).await), 0, "bit was clear");
    assert_eq!(int_of(&r(&a, &[b"SETBIT", b"m:uv:d1", b"100", b"1"]).await), 1, "repeat set says 1");
    assert_eq!(int_of(&r(&a, &[b"SETBIT", b"m:uv:d1", b"205", b"1"]).await), 0, "second visitor");
    assert_eq!(int_of(&r(&a, &[b"GETBIT", b"m:uv:d1", b"100"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"GETBIT", b"m:uv:d1", b"101"]).await), 0, "neighbor untouched");
    assert_eq!(int_of(&r(&a, &[b"BITCOUNT", b"m:uv:d1"]).await), 2, "UV is 2, repeats free");

    // d) Hot-endpoint board: pay 30, cart 25, get 12 -- ZREVRANGE must
    //    rank them exactly in that order.
    for (ep, delta) in [("/api/pay", 30), ("/api/get", 12), ("/api/cart", 25)] {
        r(&a, &[b"ZINCRBY", b"m:hot", delta.to_string().as_bytes(), ep.as_bytes()]).await;
    }
    let board = rf(&a, &[b"ZREVRANGE", b"m:hot", b"0", b"-1", b"WITHSCORES"]).await;
    let rank = |s: &str, board: &str| board.find(s).unwrap_or_else(|| panic!("{s} missing: {board}"));
    assert!(rank("/api/pay", &board) < rank("/api/cart", &board), "{board}");
    assert!(rank("/api/cart", &board) < rank("/api/get", &board), "{board}");

    // e) Raw samples ride the stream; XRANGE replays them and the
    //    recount must equal the bucket hits total.
    let samples = ["api pay", "api get", "api cart", "db q"];
    for s in samples {
        let ev = r(&a, &[b"XADD", b"metrics/raw", b"*", b"sample", s.as_bytes()]).await;
        assert!(scenario_common::bulk_at(&ev, 0).contains('-'), "sample {s}");
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"metrics/raw"]).await), 4);
    let replay = rf(&a, &[b"XRANGE", b"metrics/raw", b"-", b"+"]).await;
    let replayed = stream_ids(&replay);
    assert_eq!(replayed.len(), 4, "replay recount == sample count: {replay}");
    for s in samples {
        assert!(replay.contains(s), "{s} in replay: {replay}");
    }

    // f) SQL rollup: one row per metric, reconciled against RESP.
    run(&mut c, "INSERT INTO rollup (metric, val) VALUES ('hits:api', 3), ('hits:db', 1), ('uv:d1', 2)").await;
    let n = |v: &MVal| match v {
        MVal::Int(i) => *i,
        MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("cell {other:?}"),
    };
    assert_eq!(n(&one(&mut c, "SELECT val FROM rollup WHERE metric = 'hits:api'").await), 3);
    assert_eq!(
        ints(&mut c, "SELECT val FROM rollup WHERE metric IN ('hits:api','hits:db') ORDER BY metric").await,
        vec![3, 1],
        "buckets roll up (api 3, db 1 in the IN-list order)"
    );
    // Three planes, one truth: buckets + UV == SQL total == raw count + UV.
    let resp_total = 3 + 1 + int_of(&r(&a, &[b"BITCOUNT", b"m:uv:d1"]).await);
    let sql_total = n(&one(&mut c, "SELECT SUM(val) FROM rollup").await);
    assert_eq!(sql_total, 6);
    assert_eq!(resp_total, 6, "RESP planes reconcile with SQL");
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"metrics/raw"]).await) + int_of(&r(&a, &[b"BITCOUNT", b"m:uv:d1"]).await),
        sql_total,
        "stream replay + UV == rollup"
    );
}
