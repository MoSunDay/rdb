//! Multi-store scenario #22 -- hotel booking with bitmap occupancy
//! calendars: each room's 30-day availability is a SETBIT bitmap (first
//! set :0, re-set :1, BITCOUNT = booked nights, GETBIT spot checks), the
//! rack rate board is a ZSET queried by price window (ZRANGEBYSCORE),
//! amenities are Sets intersected with SINTER, and the folio lives in
//! SQL behind a UNIQUE night key ("R3-10") so a replayed night is refused at
//! COMMIT (1062-at-commit) without touching the calendar.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, int_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn hotel_bitmap_calendar_price_board_and_folio() {
    let _serial = scenario_common::serial_guard().await;
    // a) One node: RESP + MySQL front; the folio dedupes room+night.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("hotel22", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE folio (id BIGINT PRIMARY KEY, night VARCHAR(16) NOT NULL, \
         guest VARCHAR(8) NOT NULL, UNIQUE KEY uk_night (night))",
    )
    .await;

    // b) Rack rates: R1 880, R2 1280, R3 980; the budget window
    //    [800, 1000] must offer exactly R1 and R3.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"ZADD",
                    b"hotel:rate",
                    b"880",
                    b"R1",
                    b"1280",
                    b"R2",
                    b"980",
                    b"R3"
                ]
            )
            .await
        ),
        3
    );
    let budget = rf(&a, &[b"ZRANGEBYSCORE", b"hotel:rate", b"800", b"1000"]).await;
    assert!(
        budget.contains("R1") && budget.contains("R3") && !budget.contains("R2"),
        "{budget}"
    );
    assert_eq!(rb(&a, &[b"ZSCORE", b"hotel:rate", b"R2"]).await, "1280");

    // c) Occupancy bitmaps: R1 already holds nights 1-3 (a repeat set
    //    answers :1 and never inflates), R2 holds 1-2, R3 is empty.
    for day in [1, 2, 3] {
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[b"SETBIT", b"hotel:occ:R1", day.to_string().as_bytes(), b"1"]
                )
                .await
            ),
            0
        );
    }
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"hotel:occ:R1", b"1", b"1"]).await),
        1,
        "night 1 already held"
    );
    for day in [1, 2] {
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[b"SETBIT", b"hotel:occ:R2", day.to_string().as_bytes(), b"1"]
                )
                .await
            ),
            0
        );
    }
    assert_eq!(
        int_of(&r(&a, &[b"BITCOUNT", b"hotel:occ:R1"]).await),
        3,
        "three nights"
    );
    assert_eq!(int_of(&r(&a, &[b"BITCOUNT", b"hotel:occ:R2"]).await), 2);
    assert_eq!(
        int_of(&r(&a, &[b"BITCOUNT", b"hotel:occ:R3"]).await),
        0,
        "R3 wide open"
    );
    assert_eq!(int_of(&r(&a, &[b"GETBIT", b"hotel:occ:R1", b"2"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"GETBIT", b"hotel:occ:R1", b"4"]).await), 0);

    // d) g1 books R3 nights 10+11: folio rows land (single-column
    //    UNIQUE on the synthetic night key "R3-10" -- this SQL layer
    //    has no multi-column indexes), then the calendar flips bits.
    run(
        &mut c,
        "INSERT INTO folio (id, night, guest) VALUES (1, 'R3-10', 'g1'), (2, 'R3-11', 'g1')",
    )
    .await;
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"hotel:occ:R3", b"10", b"1"]).await),
        0,
        "night 10 was free"
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"hotel:occ:R3", b"11", b"1"]).await),
        0
    );
    assert_eq!(int_of(&r(&a, &[b"BITCOUNT", b"hotel:occ:R3"]).await), 2);

    // e) A replay of night 10 under another guest stages fine but is
    //    refused at COMMIT; the count AND the calendar stay put.
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "INSERT INTO folio (id, night, guest) VALUES (9, 'R3-10', 'g9')",
    )
    .await;
    let err = c
        .query_drop("COMMIT")
        .await
        .expect_err("double-booked night must be refused at COMMIT");
    let msg = err.to_string();
    assert!(
        msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"),
        "{msg}"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM folio").await,
        vec![2],
        "folio conserved"
    );
    assert_eq!(
        int_of(&r(&a, &[b"BITCOUNT", b"hotel:occ:R3"]).await),
        2,
        "calendar conserved"
    );

    // f) Amenities: R1 has wifi+pool+gym, R3 has wifi+pool -- the
    //    shared set is exactly wifi+pool.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{amen}:R1", b"wifi", b"pool", b"gym"]).await),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{amen}:R3", b"wifi", b"pool"]).await),
        2
    );
    let shared = rf(&a, &[b"SINTER", b"{amen}:R1", b"{amen}:R3"]).await;
    assert!(
        shared.contains("wifi") && shared.contains("pool") && !shared.contains("gym"),
        "{shared}"
    );
    let amens = rf(&a, &[b"SMEMBERS", b"{amen}:R3"]).await;
    assert!(
        amens.contains("wifi") && amens.contains("pool") && !amens.contains("gym"),
        "{amens}"
    );

    // g) Reconciliation: folio rows == booked nights on the calendar;
    //    revenue = rate x nights for R3 = 1960; each night audited.
    let nights = int_of(&r(&a, &[b"BITCOUNT", b"hotel:occ:R3"]).await) as i64;
    let rows = ints(&mut c, "SELECT COUNT(*) FROM folio").await;
    assert_eq!(rows, vec![nights], "one calendar bit per folio row");
    let rate = rb(&a, &[b"ZSCORE", b"hotel:rate", b"R3"])
        .await
        .parse::<i64>()
        .unwrap();
    assert_eq!(rate * nights, 1960, "R3 revenue");
    for day in [10, 11] {
        let stay = r(
            &a,
            &[
                b"XADD",
                b"hotel/stays",
                b"*",
                b"room",
                b"R3",
                b"day",
                day.to_string().as_bytes(),
                b"guest",
                b"g1",
            ],
        )
        .await;
        assert!(
            scenario_common::bulk_at(&stay, 0).contains('-'),
            "stay night {day}"
        );
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"hotel/stays"]).await), 2);
}
