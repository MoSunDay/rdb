//! Multi-store scenario #32 -- a travel-booking saga on the SQL plane:
//! flight + hotel + car legs stage inside one transaction, a car leg
//! that fails its business check is compensated by ROLLBACK TO
//! SAVEPOINT (partial undo, the rest of the saga survives), RELEASE
//! retires a marker for good, an unknown marker is a MySQL-1305-shaped
//! error, and the final itinerary report is a three-way LEFT JOIN with
//! a NULL leg for the no-car booking. One REAL node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

/// Render one SQL cell: NULL keeps its own spelling so LEFT JOIN gaps
/// are distinguishable from empty strings.
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        MVal::Double(d) => d.to_string(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn travel_saga_partial_compensation_and_left_join_report() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("saga32", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Inventory: seats/rooms/plates are all single-column UNIQUE.
    ddl(
        &mut c,
        "CREATE TABLE flights (id BIGINT PRIMARY KEY, seat VARCHAR(8) NOT NULL, \
         UNIQUE KEY uk_seat (seat))",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE hotels (id BIGINT PRIMARY KEY, room VARCHAR(8) NOT NULL, \
         UNIQUE KEY uk_room (room))",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE cars (id BIGINT PRIMARY KEY, plate VARCHAR(8) NOT NULL, \
         UNIQUE KEY uk_plate (plate))",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE itin (pid BIGINT PRIMARY KEY, flight_id BIGINT NOT NULL, \
         hotel_id BIGINT NOT NULL, car_id BIGINT NULL)",
    )
    .await;

    // b) Happy saga: three legs stage, the car sits behind a savepoint
    //    that is RELEASEd on success -- the marker retires but the
    //    staged row stays.
    run(&mut c, "BEGIN").await;
    run(&mut c, "INSERT INTO flights (id, seat) VALUES (1, '11A')").await;
    run(&mut c, "INSERT INTO hotels (id, room) VALUES (1, '402')").await;
    run(&mut c, "SAVEPOINT sp_car").await;
    run(&mut c, "INSERT INTO cars (id, plate) VALUES (1, 'B-1')").await;
    run(&mut c, "RELEASE SAVEPOINT sp_car").await;
    run(&mut c, "INSERT INTO itin VALUES (100, 1, 1, 1)").await;
    let err = c
        .query_drop("ROLLBACK TO SAVEPOINT sp_car")
        .await
        .expect_err("released marker is gone");
    assert!(
        err.to_string().contains("SAVEPOINT sp_car does not exist"),
        "1305-shaped: {err}"
    );
    run(&mut c, "COMMIT").await;

    // c) Compensated saga: the wanted plate is rejected by the fleet
    //    check mid-transaction; ROLLBACK TO SAVEPOINT undoes ONLY the
    //    car leg while the flight/hotel legs survive to COMMIT.
    run(&mut c, "BEGIN").await;
    run(&mut c, "INSERT INTO flights (id, seat) VALUES (2, '12A')").await;
    run(&mut c, "INSERT INTO hotels (id, room) VALUES (2, '503')").await;
    run(&mut c, "SAVEPOINT sp_car").await;
    run(&mut c, "INSERT INTO cars (id, plate) VALUES (2, 'X-9')").await;
    let fleet = rows(&mut c, "SELECT id FROM cars WHERE plate LIKE 'X-%'").await;
    assert_eq!(fleet.len(), 1, "staged X-plate visible inside the tx");
    // ...the fleet office refuses all X-plates: compensate the leg.
    run(&mut c, "ROLLBACK TO SAVEPOINT sp_car").await;
    let gone = rows(&mut c, "SELECT id FROM cars WHERE plate LIKE 'X-%'").await;
    assert!(gone.is_empty(), "compensated leg is undone in-tx");
    run(&mut c, "INSERT INTO cars (id, plate) VALUES (3, 'B-3')").await;
    run(&mut c, "INSERT INTO itin VALUES (200, 2, 2, 3)").await;
    run(&mut c, "COMMIT").await;
    let cars = rows(&mut c, "SELECT plate FROM cars ORDER BY id").await;
    assert_eq!(
        cars.iter().map(|r| cell(&r[0])).collect::<Vec<_>>(),
        vec!["B-1", "B-3"],
        "X-9 never landed"
    );

    // d) The no-car traveller books flight+hotel only: a NULL car_id
    //    leg in the report's LEFT JOIN.
    run(&mut c, "BEGIN").await;
    run(&mut c, "INSERT INTO flights (id, seat) VALUES (4, '14A')").await;
    run(&mut c, "INSERT INTO hotels (id, room) VALUES (4, '604')").await;
    run(&mut c, "INSERT INTO itin VALUES (300, 4, 4, NULL)").await;
    run(&mut c, "COMMIT").await;

    // e) Unknown markers never rollback anything (1305-shaped), and a
    //    plain unknown name errors identically outside any savepoint.
    let err = c
        .query_drop("ROLLBACK TO SAVEPOINT nope")
        .await
        .expect_err("unknown marker");
    assert!(
        err.to_string().contains("SAVEPOINT nope does not exist"),
        "1305-shaped: {err}"
    );

    // f) The itinerary report: three-way LEFT JOIN keeps the car-less
    //    traveller with a NULL plate.
    let report = rows(
        &mut c,
        "SELECT i.pid, f.seat, h.room, c.plate FROM itin i \
         LEFT JOIN flights f ON f.id = i.flight_id \
         LEFT JOIN hotels h ON h.id = i.hotel_id \
         LEFT JOIN cars c ON c.id = i.car_id \
         ORDER BY i.pid",
    )
    .await;
    let rendered: Vec<Vec<String>> = report
        .iter()
        .map(|r| r.iter().map(cell).collect())
        .collect();
    assert_eq!(
        rendered,
        vec![
            vec!["100", "11A", "402", "B-1"],
            vec!["200", "12A", "503", "B-3"],
            vec!["300", "14A", "604", "<NULL>"],
        ],
        "left-join report"
    );
}
