//! Multi-store scenario #53 -- an API rate limiter: the token bucket
//! counts hits with INCR inside an EXPIREAT'd window (TTL proves the
//! clock), GETSET swaps the window open atomically (the old count is
//! the reply), DECR/DECRBY/INCRBY re-balance borrowed tokens (pure
//! i64 arithmetic, may go negative), and the 429 note grows with
//! APPEND (STRLEN sizes it, GETRANGE excerpts it, both ends
//! inclusive). A MySQL plan-quota ledger mirrors the ceilings. One
//! REAL node, RESP + SQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rb, FrontSpec};

#[tokio::test]
async fn token_bucket_counts_resets_and_rebalances() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("rl53", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Two hits land in the bucket; the window key rides an
    //    EXPIREAT deadline.
    assert_eq!(int_of(&r(&a, &[b"INCR", b"{rl}hits"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"INCR", b"{rl}hits"]).await), 2);
    scenario_common::ok(&r(&a, &[b"SET", b"{rl}win", b"60s"]).await, "set window");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIREAT", b"{rl}win", b"4102444800"]).await),
        1
    );
    assert!(
        int_of(&r(&a, &[b"TTL", b"{rl}win"]).await) > 0,
        "window clock ticking"
    );

    // b) Rollover: GETSET swaps in the fresh count and answers the
    //    old one in one round-trip.
    scenario_common::ok(
        &r(&a, &[b"SET", b"{rl}hits", b"99"]).await,
        "prime the bucket",
    );
    assert_eq!(rb(&a, &[b"GETSET", b"{rl}hits", b"0"]).await, "99");
    assert_eq!(rb(&a, &[b"GET", b"{rl}hits"]).await, "0");

    // c) Token re-balance: DECR/DECRBY pay borrowed tokens back (the
    //    counter may cross zero), INCRBY draws a burst again.
    assert_eq!(int_of(&r(&a, &[b"DECR", b"{rl}hits"]).await), -1);
    assert_eq!(int_of(&r(&a, &[b"DECRBY", b"{rl}hits", b"5"]).await), -6);
    assert_eq!(int_of(&r(&a, &[b"INCRBY", b"{rl}hits", b"7"]).await), 1);

    // d) The 429 note: APPEND grows it, STRLEN sizes it, GETRANGE
    //    excerpts the code.
    scenario_common::ok(&r(&a, &[b"SET", b"{rl}note", b"429"]).await, "note");
    assert_eq!(
        int_of(&r(&a, &[b"APPEND", b"{rl}note", b" retry-after 30s"]).await),
        19
    );
    assert_eq!(int_of(&r(&a, &[b"STRLEN", b"{rl}note"]).await), 19);
    assert_eq!(rb(&a, &[b"GETRANGE", b"{rl}note", b"0", b"2"]).await, "429");

    // e) The plan-quota ledger mirrors the limiter ceilings.
    ddl(
        &mut c,
        "CREATE TABLE plan_quota (plan VARCHAR(16) PRIMARY KEY, rps INT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO plan_quota (plan, rps) VALUES ('free', 10), ('pro', 100)",
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT rps FROM plan_quota WHERE plan = 'pro'").await,
        vec![100]
    );
}
