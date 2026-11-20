//! Multi-store scenario #29 -- a ground-operations desk for one
//! aircraft's day: the departure board is a Redis ZSET scored by
//! schedule minute (window queries + departed-flight removal), the
//! flight legs live in MySQL behind a UNIQUE flight number, a
//! turnaround is GUARDED SQL (the return leg can only flip to departed
//! once its tail actually landed -- a WHERE-guarded UPDATE that simply
//! touches zero rows otherwise), and a cancelled leg's passengers
//! rebook in one multi-row UPDATE reconciled with an IN(...) read.
//! One REAL node, MySQL front (+ RESP board).

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rf, FrontSpec};

#[tokio::test]
async fn flight_ground_board_turnaround_and_rebooking() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("ground29", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The day of tail N301: out-and-back plus a third leg.
    ddl(
        &mut c,
        "CREATE TABLE flights (id BIGINT PRIMARY KEY, flight_no VARCHAR(8) NOT NULL, \
         tail VARCHAR(8), origin VARCHAR(4), dest VARCHAR(4), gate VARCHAR(4), \
         sched INT, status VARCHAR(12), UNIQUE KEY uk_flight (flight_no))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO flights (id, flight_no, tail, origin, dest, gate, sched, status) VALUES \
         (1, 'FL201', 'N301', 'PEK', 'SHA', 'C22', 30, 'scheduled'), \
         (2, 'FL202', 'N301', 'SHA', 'PEK', 'C08', 200, 'scheduled'), \
         (3, 'FL203', 'N302', 'PEK', 'CAN', 'D01', 45, 'scheduled')",
    )
    .await;

    // b) The departure board: ZSET scored by schedule minute.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"board:dep", b"30", b"FL201", b"45", b"FL203", b"200", b"FL202"]).await),
        3,
        "board loaded"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"board:dep"]).await), 3);
    // b0) The morning window (minutes 0..60) lists exactly the two
    //     early legs in score order.
    let morning = rf(&a, &[b"ZRANGEBYSCORE", b"board:dep", b"0", b"60"]).await;
    assert!(morning.contains("FL201") && morning.contains("FL203") && !morning.contains("FL202"), "{morning}");

    // c) Turnaround guard: FL202 may only depart once tail N301 has
    //    LANDED. With FL201 still 'scheduled', the guarded UPDATE
    //    matches zero rows -- FL202 stays scheduled.
    run(
        &mut c,
        "UPDATE flights SET status = 'departed' \
         WHERE flight_no = 'FL202' AND status = 'awaiting_tail'",
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM flights WHERE flight_no = 'FL202' AND status = 'scheduled'").await,
        vec![1],
        "guard touched nothing: FL202 still scheduled"
    );

    // d) FL201 actually flies and lands; NOW the return leg flips.
    run(&mut c, "UPDATE flights SET status = 'departed' WHERE flight_no = 'FL201'").await;
    run(&mut c, "UPDATE flights SET status = 'landed' WHERE flight_no = 'FL201'").await;
    run(&mut c, "UPDATE flights SET status = 'awaiting_tail' WHERE flight_no = 'FL202'").await;
    run(
        &mut c,
        "UPDATE flights SET status = 'departed' \
         WHERE flight_no = 'FL202' AND status = 'awaiting_tail'",
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM flights WHERE flight_no = 'FL202' AND status = 'departed'").await,
        vec![1],
        "return leg finally departed"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM flights WHERE tail = 'N301' AND status = 'landed'").await,
        vec![1],
        "N301 ends its day on the ground at PEK"
    );

    // e) Board maintenance: departed legs leave the board, and FL203
    //    is cancelled off it too; one leg remains.
    assert_eq!(
        int_of(&r(&a, &[b"ZREM", b"board:dep", b"FL201", b"FL202", b"FL203"]).await),
        3,
        "board clear"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"board:dep"]).await), 0, "board empty");

    // f) Rebooking wave: FL203's three passengers move onto the
    //    return legs (one multi-row UPDATE rides the IN(...) list),
    //    then reconcile via IN(...).
    ddl(
        &mut c,
        "CREATE TABLE pax (id BIGINT PRIMARY KEY, name VARCHAR(8), flight_no VARCHAR(8))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO pax (id, name, flight_no) VALUES \
         (1, 'pa', 'FL203'), (2, 'pb', 'FL203'), (3, 'pc', 'FL203')",
    )
    .await;
    run(&mut c, "UPDATE pax SET flight_no = 'FL201' WHERE id IN (1, 2)").await;
    run(&mut c, "UPDATE pax SET flight_no = 'FL202' WHERE id = 3").await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM pax WHERE flight_no IN ('FL201','FL202','FL203')").await,
        vec![3],
        "everyone rebooked"
    );
}
