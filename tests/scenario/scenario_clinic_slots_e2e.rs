//! Multi-store scenario #28 -- a clinic appointment desk across THREE
//! stores: bookings land in MySQL behind a single-column UNIQUE slot
//! key (a racing patient loses the double-book at COMMIT), every
//! confirmed booking stages a DELAY reminder job that is XLEN-invisible
//! until the sweep exchanges it, and the reminder group's delivery is
//! driven by the SQL status -- a cancelled appointment sends no SMS.
//! One REAL node, MySQL + lite-delay fronts.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, int_of, ok, r, rf, stream_ids, until_contains, FrontSpec};

const SWEEP_MS: u64 = 200;

#[tokio::test]
async fn clinic_slots_double_book_and_status_driven_reminders() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        extra_yaml: format!("lite:\n  delay_sweep_ms: {SWEEP_MS}\n"),
        ..Default::default()
    };
    let (front, a) = front_world("clinic28", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The schedule: 15-minute slots, one row per slot max.
    ddl(
        &mut c,
        "CREATE TABLE appts (id BIGINT PRIMARY KEY, slot_key VARCHAR(16) NOT NULL, \
         doctor VARCHAR(8), patient VARCHAR(8), status VARCHAR(12), \
         UNIQUE KEY uk_slot (slot_key))",
    )
    .await;

    // b) Three patients book; each confirmed row stages a DELAY
    //    reminder for T-30 minutes (sweep exchanges it later).
    let bookings = [
        ("drA-0900", "drA", "p1"),
        ("drA-0915", "drA", "p2"),
        ("drB-0900", "drB", "p3"),
    ];
    for (i, (slot, doctor, patient)) in bookings.iter().enumerate() {
        run(
            &mut c,
            &format!(
                "INSERT INTO appts (id, slot_key, doctor, patient, status) \
                 VALUES ({i}, '{slot}', '{doctor}', '{patient}', 'confirmed')"
            ),
        )
        .await;
        let staged = r(
            &a,
            &[
                b"XADD",
                b"clinic/remind",
                b"*",
                b"DELAY",
                b"400",
                b"slot",
                slot.as_bytes(),
                b"patient",
                patient.as_bytes(),
            ],
        )
        .await;
        assert!(
            scenario_common::bulk_at(&staged, 0).contains('-'),
            "reminder staged for {patient}"
        );
    }
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"clinic/remind"]).await),
        0,
        "nothing due yet"
    );

    // c) p4 races p1 for drA-0900: the INSERT stages but COMMIT
    //    refuses the second booking of the same slot, so p4 gets NO
    //    reminder either.
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "INSERT INTO appts (id, slot_key, doctor, patient, status) \
         VALUES (9, 'drA-0900', 'drA', 'p4', 'confirmed')",
    )
    .await;
    let err = c
        .query_drop("COMMIT")
        .await
        .expect_err("double-book refused at COMMIT");
    let msg = err.to_string();
    assert!(
        msg.contains("1062") || msg.contains("Duplicate") || msg.contains("conflict"),
        "{msg}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"clinic/remind"]).await),
        0,
        "p4 staged no reminder"
    );

    // d) The sweep exchanges exactly the three due reminders; the
    //    group is created on the LIVE stream (the exchange re-roots
    //    the key, so groups made on the empty stream do not survive).
    until_contains(
        &a,
        &[b"XLEN", b"clinic/remind"],
        ":3",
        "reminders exchanged",
        15,
    )
    .await;
    ok(
        &r(
            &a,
            &[b"XGROUP", b"CREATE", b"clinic/remind", b"reminders", b"0-0"],
        )
        .await,
        "group",
    );

    // e) p2 cancels before the wave runs: the row flips to cancelled.
    run(
        &mut c,
        "UPDATE appts SET status = 'cancelled' WHERE patient = 'p2'",
    )
    .await;

    // f) Delivery is status-driven: confirmed patients get the SMS and
    //    flip to reminded; the cancelled booking stays silent.
    let mut sent = 0;
    for _ in 0..3 {
        let one = rf(
            &a,
            &[
                b"XREADGROUP",
                b"GROUP",
                b"reminders",
                b"w1",
                b"COUNT",
                b"1",
                b"STREAMS",
                b"clinic/remind",
                b">",
            ],
        )
        .await;
        let id = stream_ids(&one)[0].clone();
        let slot = ["drA-0900", "drA-0915", "drB-0900"]
            .iter()
            .find(|s| one.contains(*s))
            .expect("delivery names its slot");
        let cancelled = ints(
            &mut c,
            &format!(
                "SELECT COUNT(*) FROM appts WHERE slot_key = '{slot}' AND status = 'cancelled'"
            ),
        )
        .await;
        if cancelled[0] == 0 {
            sent += 1;
            run(
                &mut c,
                &format!("UPDATE appts SET status = 'reminded' WHERE slot_key = '{slot}'"),
            )
            .await;
        }
        assert_eq!(
            int_of(
                &r(
                    &a,
                    &[b"XACK", b"clinic/remind", b"reminders", id.as_bytes()]
                )
                .await
            ),
            1
        );
    }
    assert_eq!(sent, 2, "two SMS, one silent cancellation");

    // g) Reconciliation: statuses split 2 reminded / 1 cancelled, and
    //    the reminder backlog is fully acked under the FRESH ids.
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM appts WHERE status = 'reminded'"
        )
        .await,
        vec![2],
        "p1 + p3 reminded"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM appts WHERE status = 'cancelled'"
        )
        .await,
        vec![1],
        "p2 cancelled"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM appts").await,
        vec![3],
        "p4 never landed"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"clinic/remind"]).await),
        3,
        "entries stay for audit"
    );
    let pending = rf(&a, &[b"XPENDING", b"clinic/remind", b"reminders"]).await;
    assert_eq!(pending, "*3\r\n:0\r\n$-1\r\n$-1\r\n", "empty PEL summary");
}
