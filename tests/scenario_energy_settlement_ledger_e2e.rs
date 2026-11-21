//! Multi-store scenario #69 -- a building energy settlement: each
//! meter's balance rides INCRBYFLOAT (compact answers: 1.5 + 2.5
//! prints 4, and 4 + 0.25 prints 4.25), SETRANGE rebrands a meter
//! label in place, GETRANGE slices it, GETDEL closes a meter out with
//! its final reading, and the tariff table rides HINCRBYFLOAT with
//! HSTRLEN sizing the field on read. MySQL settles with a GROUP BY
//! rollup. One REAL node, RESP + MySQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, ok, r, rb, rt, FrontSpec};

#[tokio::test]
async fn float_ledger_rebrand_and_rollup() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("energy69", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Meter balances grow in floats; the answers stay compact.
    assert_eq!(rb(&a, &[b"INCRBYFLOAT", b"{en}bal:m1", b"1.5"]).await, "1.5");
    assert_eq!(rb(&a, &[b"INCRBYFLOAT", b"{en}bal:m1", b"2.5"]).await, "4", "compact, not 4.0");
    assert_eq!(rb(&a, &[b"INCRBYFLOAT", b"{en}bal:m1", b"0.25"]).await, "4.25");
    assert!(rt(&a, &[b"INCRBYFLOAT", b"{en}bal:m1", b"abc"]).await.starts_with("-ERR"), "not a float");
    assert_eq!(int_of(&r(&a, &[b"HSET", b"{en}hold", b"f", b"v"]).await), 1);
    assert!(rt(&a, &[b"INCRBYFLOAT", b"{en}hold", b"1"]).await.starts_with("-WRONGTYPE"), "hash refuses string math");

    // b) Rebranding: SETRANGE patches the label in place, GETRANGE
    //    slices it from both ends.
    ok(&r(&a, &[b"SET", b"{en}label:m1", b"east-wing-01"]).await, "label");
    assert_eq!(int_of(&r(&a, &[b"SETRANGE", b"{en}label:m1", b"0", b"west"]).await), 12);
    assert_eq!(rb(&a, &[b"GET", b"{en}label:m1"]).await, "west-wing-01");
    assert_eq!(rb(&a, &[b"GETRANGE", b"{en}label:m1", b"0", b"3"]).await, "west");
    assert_eq!(rb(&a, &[b"GETRANGE", b"{en}label:m1", b"-3", b"-1"]).await, "-01");

    // c) Tariffs: HINCRBYFLOAT lands float fields, HSTRLEN sizes one.
    assert_eq!(rb(&a, &[b"HINCRBYFLOAT", b"{en}tariff", b"offpeak", b"0.5"]).await, "0.5");
    assert_eq!(rb(&a, &[b"HINCRBYFLOAT", b"{en}tariff", b"offpeak", b"1.5"]).await, "2");
    assert_eq!(int_of(&r(&a, &[b"HSTRLEN", b"{en}tariff", b"offpeak"]).await), 1, "the compact 1-char answer");

    // d) Close-out: GETDEL returns the final reading and removes the
    //    meter; a second read misses.
    ok(&r(&a, &[b"SET", b"{en}final:m2", b"812.5"]).await, "final");
    assert_eq!(rb(&a, &[b"GETDEL", b"{en}final:m2"]).await, "812.5");
    assert_eq!(rt(&a, &[b"GET", b"{en}final:m2"]).await, "$-1");

    // e) MySQL settles the month with a GROUP BY rollup.
    ddl(
        &mut c,
        "CREATE TABLE readings (meter INT NOT NULL, seq INT NOT NULL, \
         kwh BIGINT NOT NULL, PRIMARY KEY (meter, seq))",
    )
    .await;
    run(&mut c, "INSERT INTO readings (meter, seq, kwh) VALUES (1, 1, 4), (1, 2, 1), (2, 1, 812)").await;
    let sums = ints(&mut c, "SELECT SUM(kwh) FROM readings GROUP BY meter ORDER BY meter").await;
    assert_eq!(sums, vec![5, 812], "group rollup: {sums:?}");
}
