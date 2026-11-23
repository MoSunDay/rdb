//! Multi-store scenario #17 -- ticket seat holds: taken seats are a Set
//! (SADD :3 / repeat :0), each confirmed seat takes a `SET NX PX` lease
//! (double-lease nil), the booking record lives in SQL behind a
//! UNIQUE(seat) so a rival booking of a taken seat is refused at COMMIT
//! with the count conserved, the waitlist is a FIFO (RPUSH + LPOP),
//! and an unpaid seat is released by a DELAY-staged timeout job: the
//! sweep exchanges it (XLEN poll), the janitor group consumes it, and
//! only then SREM drops the hold (:1).

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{
    bulk_at, front_world, int_of, ok, r, rb, rf, stream_ids, until_contains, FrontSpec,
};

const SWEEP_MS: u64 = 200;

fn spec() -> FrontSpec {
    FrontSpec {
        mysql: true,
        extra_yaml: format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n"),
        ..Default::default()
    }
}

#[tokio::test]
async fn ticket_seats_holds_waitlist_and_timeout_release() {
    let _serial = scenario_common::serial_guard().await;
    let (front, a) = front_world("tix17", &spec()).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE bookings (id BIGINT PRIMARY KEY, seat VARCHAR(8) NOT NULL, \
         holder VARCHAR(8) NOT NULL, UNIQUE KEY uk_seat (seat))",
    )
    .await;

    // a) Three seats held; a repeat SADD of a held seat adds nothing.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"tix:holds", b"S1", b"S2", b"S3"]).await),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"tix:holds", b"S1"]).await),
        0,
        "already held"
    );
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"tix:holds"]).await), 3);

    // b) A confirmed seat takes a lease; a rival lease is nil.
    ok(
        &r(
            &a,
            &[b"SET", b"tix:lease:S1", b"u1", b"NX", b"PX", b"30000"],
        )
        .await,
        "u1 leases S1",
    );
    let rival = r(
        &a,
        &[b"SET", b"tix:lease:S1", b"u9", b"NX", b"PX", b"30000"],
    )
    .await;
    assert!(rival.starts_with(b"$-1"), "rival lease refused: {rival:?}");

    // c) Two bookings land in SQL; a rival booking of taken S1 stages
    //    fine but COMMIT refuses the duplicate seat -- count conserved.
    run(
        &mut c,
        "INSERT INTO bookings (id, seat, holder) VALUES (1, 'S1', 'u1'), (2, 'S2', 'u2')",
    )
    .await;
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "INSERT INTO bookings (id, seat, holder) VALUES (9, 'S1', 'u9')",
    )
    .await;
    let err = c
        .query_drop("COMMIT")
        .await
        .expect_err("rival seat booking must be refused at COMMIT");
    let msg = err.to_string();
    assert!(
        msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"),
        "dup seat refusal: {msg}"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM bookings").await,
        vec![2],
        "count conserved"
    );

    // d) The waitlist is FIFO: RPUSH two, LPOP serves them in order.
    assert_eq!(
        int_of(&r(&a, &[b"RPUSH", b"tix:wait", b"u4", b"u5"]).await),
        2
    );
    assert_eq!(
        rb(&a, &[b"LPOP", b"tix:wait"]).await,
        "u4",
        "first in, first out"
    );
    assert_eq!(rb(&a, &[b"LPOP", b"tix:wait"]).await, "u5");
    assert_eq!(
        int_of(&r(&a, &[b"LLEN", b"tix:wait"]).await),
        0,
        "waitlist drained"
    );

    // e) S3 went unpaid: a DELAY-staged timeout job releases it. Staged
    //    rows are invisible (XLEN 0) until the sweep exchanges them.
    let staged = r(
        &a,
        &[
            b"XADD",
            b"tix/expire",
            b"*",
            b"DELAY",
            b"400",
            b"seat",
            b"S3",
        ],
    )
    .await;
    assert!(bulk_at(&staged, 0).contains('-'), "staged receipt minted");
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"tix/expire"]).await),
        0,
        "staged invisible"
    );
    ok(
        &r(
            &a,
            &[b"XGROUP", b"CREATE", b"tix/expire", b"janitor", b"0-0"],
        )
        .await,
        "janitor group",
    );
    until_contains(
        &a,
        &[b"XLEN", b"tix/expire"],
        ":1",
        "timeout job exchanged",
        15,
    )
    .await;

    // f) The janitor consumes the job under its FRESH minted id and
    //    only then drops the hold; S1/S2 keep theirs.
    let job = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"janitor",
            b"j1",
            b"STREAMS",
            b"tix/expire",
            b">",
        ],
    )
    .await;
    assert!(job.contains("S3"), "{job}");
    let ids = stream_ids(&job);
    assert_eq!(ids.len(), 1);
    assert_ne!(ids[0], bulk_at(&staged, 0), "delay mints fresh ids");
    assert_eq!(
        int_of(&r(&a, &[b"SREM", b"tix:holds", b"S3"]).await),
        1,
        "hold released"
    );
    assert_eq!(
        int_of(&r(&a, &[b"SCARD", b"tix:holds"]).await),
        2,
        "S1+S2 still held"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XACK", b"tix/expire", b"janitor", ids[0].as_bytes()]).await),
        1
    );

    // g) Reconciliation: RESP holds == SQL booked seats, the lease
    //    survives, and the exchanged stream kept exactly one entry.
    let held = ints(&mut c, "SELECT COUNT(*) FROM bookings").await;
    assert_eq!(held[0], 2);
    assert_eq!(
        int_of(&r(&a, &[b"SISMEMBER", b"tix:holds", b"S1"]).await),
        1,
        "booked seats still held"
    );
    assert_eq!(
        rb(&a, &[b"GET", b"tix:lease:S1"]).await,
        "u1",
        "lease alive"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"tix/expire"]).await),
        1,
        "job retained post-ack"
    );
}
