//! Multi-store scenario #64 -- a deli counter queue: LINSERT cuts a
//! regular ahead of / behind a named pivot (and answers 0/-1 for a
//! missing key or pivot), LSET re-calls a ticket in place (+OK, index
//! errors refused), LINDEX peeks head/tail with nil past the ends,
//! LTRIM keeps the served window, LPUSHX/RPUSHX refuse to create, and
//! LREM pulls cancellations. MySQL mirrors the final counter order.
//! One REAL node, RESP + MySQL.

mod common;
mod scenario_common;

use common::mysql::{col, connect_root, ddl, run, s};
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn counter_cuts_recalls_and_trims() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("deli64", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The morning line: RPUSH seeds four tickets.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{dq}line", b"t1", b"t2", b"t4", b"t5"]).await), 4);

    // b) A regular cuts in: LINSERT BEFORE the pivot answers the new
    //    length; a missing pivot answers -1 and a missing key 0; a
    //    bogus WHERE is a syntax error.
    assert_eq!(int_of(&r(&a, &[b"LINSERT", b"{dq}line", b"BEFORE", b"t2", b"t0"]).await), 5);
    assert_eq!(int_of(&r(&a, &[b"LINSERT", b"{dq}line", b"AFTER", b"t4", b"t4b"]).await), 6);
    assert_eq!(int_of(&r(&a, &[b"LINSERT", b"{dq}line", b"BEFORE", b"t99", b"x"]).await), -1, "no pivot");
    assert_eq!(int_of(&r(&a, &[b"LINSERT", b"{dq}noline", b"BEFORE", b"t2", b"x"]).await), 0, "no key");
    assert_eq!(rt(&a, &[b"LINSERT", b"{dq}line", b"sideways", b"t2", b"x"]).await, "-ERR syntax error");

    // c) The board now reads t0 t1 t2 t4 t4b t5; LSET re-calls t2's
    //    owner by index (+OK), out-of-range and missing keys refuse.
    let board = rf(&a, &[b"LRANGE", b"{dq}line", b"0", b"-1"]).await;
    for (i, want) in ["t1", "t0", "t2", "t4", "t4b", "t5"].iter().enumerate() {
        assert_eq!(&bulk_at(board.as_bytes(), i), want, "slot {i}: {board}");
    }
    assert_eq!(rt(&a, &[b"LSET", b"{dq}line", b"2", b"t2r"]).await, "+OK");
    assert_eq!(rb(&a, &[b"LINDEX", b"{dq}line", b"2"]).await, "t2r");
    assert_eq!(rt(&a, &[b"LSET", b"{dq}line", b"9", b"x"]).await, "-ERR index out of range");
    assert_eq!(rt(&a, &[b"LSET", b"{dq}noline", b"0", b"x"]).await, "-ERR no such key");

    // d) LINDEX peeks both ends; past the ends is a nil bulk.
    assert_eq!(rb(&a, &[b"LINDEX", b"{dq}line", b"0"]).await, "t1");
    assert_eq!(rb(&a, &[b"LINDEX", b"{dq}line", b"-1"]).await, "t5");
    assert_eq!(rt(&a, &[b"LINDEX", b"{dq}line", b"6"]).await, "$-1");
    assert_eq!(rt(&a, &[b"LINDEX", b"{dq}line", b"-7"]).await, "$-1");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{dq}line"]).await), 6);

    // e) LTRIM keeps the serving window [1,4]; LPUSHX/RPUSHX refuse to
    //    create the missing pickup list; LREM pulls one cancellation.
    assert_eq!(rt(&a, &[b"LTRIM", b"{dq}line", b"1", b"4"]).await, "+OK");
    let kept = rf(&a, &[b"LRANGE", b"{dq}line", b"0", b"-1"]).await;
    for (i, want) in ["t0", "t2r", "t4", "t4b"].iter().enumerate() {
        assert_eq!(&bulk_at(kept.as_bytes(), i), want, "after trim slot {i}: {kept}");
    }
    assert_eq!(rt(&a, &[b"LTRIM", b"{dq}noline", b"0", b"1"]).await, "+OK", "trim of nothing");
    assert_eq!(int_of(&r(&a, &[b"LPUSHX", b"{dq}pickup", b"x"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"RPUSHX", b"{dq}pickup", b"x"]).await), 0);
    assert_eq!(rt(&a, &[b"TYPE", b"{dq}pickup"]).await, "+none", "never created");
    assert_eq!(int_of(&r(&a, &[b"LREM", b"{dq}line", b"1", b"t4"]).await), 1);

    // f) MySQL mirrors the final counter order.
    ddl(
        &mut c,
        "CREATE TABLE counter (pos INT PRIMARY KEY, ticket VARCHAR(8) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO counter (pos, ticket) VALUES (1,'t0'), (2,'t2r'), (3,'t4b')").await;
    let order = col(&mut c, "SELECT ticket FROM counter ORDER BY pos").await;
    assert_eq!(order, vec![s("t0"), s("t2r"), s("t4b")]);
}
