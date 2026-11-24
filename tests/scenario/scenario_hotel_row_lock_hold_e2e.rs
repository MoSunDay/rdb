//! Batch 11 scenario #122 -- a hotel front desk holds a room row across
//! a check-in: `SELECT ... FOR UPDATE` latches the matched pk until
//! COMMIT, a second session's locking read on the SAME row fails fast
//! with MySQL's 1205 (`Lock wait timeout exceeded`) while a different
//! row stays lockable, COMMIT releases the latch so the retry lands,
//! `FOR SHARE` composes across two sessions, and the non-blocking
//! spellings (`NOWAIT` / `SKIP LOCKED`) stay 1235. Ten hold/release
//! rounds stay inside the budget. One REAL node, MySQL front.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, one, rows, run, server_error, s, ER_NOT_SUPPORTED_YET};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, FrontSpec};

/// MySQL 1205 (ER_LOCK_WAIT_TIMEOUT): the code the latch conflict
/// answers with (not in the shared const set yet).
const ER_LOCK_WAIT_TIMEOUT: u16 = 1205;

/// Locking-read cell of one row, asserted against `want`.
async fn locked(conn: &mut mysql_async::Conn, room: &str, clause: &str, want: &str) {
    let sql = format!("SELECT guest FROM rooms WHERE room = '{room}' {clause}");
    let got = one(conn, &sql).await;
    assert_eq!(got, s(want), "{sql}");
}

#[tokio::test]
async fn hotel_room_row_lock_hold_conflict_and_release() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("lock122", &spec).await;
    let mut a = connect_root(&front.node).await;
    let mut b = connect_root(&front.node).await;

    // a) Two rooms; the desk session A checks room1 in and keeps the
    //    row latched for the rest of its txn.
    ddl(
        &mut a,
        "CREATE TABLE rooms (room VARCHAR(16) PRIMARY KEY, guest VARCHAR(32))",
    )
    .await;
    run(
        &mut a,
        "INSERT INTO rooms (room, guest) VALUES ('room1', 'alice'), ('room2', 'bob')",
    )
    .await;
    a.query_drop("BEGIN").await.expect("A begin");
    locked(&mut a, "room1", "FOR UPDATE", "alice").await;

    // b) Session B's locking read on the SAME row conflicts: rdb never
    //    sleeps through innodb_lock_wait_timeout, it answers MySQL's
    //    1205 message shape immediately (all-or-nothing: B holds
    //    nothing afterwards). A different row stays lockable.
    b.query_drop("BEGIN").await.expect("B begin");
    let e = server_error(
        &mut b,
        "SELECT guest FROM rooms WHERE room = 'room1' FOR UPDATE",
    )
    .await;
    assert_eq!(e.code, ER_LOCK_WAIT_TIMEOUT, "{e:?}");
    assert!(
        e.message.contains("Lock wait timeout exceeded"),
        "{}",
        e.message
    );
    locked(&mut b, "room2", "FOR UPDATE", "bob").await;

    // c) COMMIT releases A's latches, so B's retry on room1 lands.
    a.query_drop("COMMIT").await.expect("A commit");
    locked(&mut b, "room1", "FOR UPDATE", "alice").await;
    b.query_drop("ROLLBACK").await.expect("B rollback");

    // d) FOR SHARE composes: both sessions read the same row while
    //    both txns are open (a shared latch excludes nobody).
    a.query_drop("BEGIN").await.expect("A begin share");
    b.query_drop("BEGIN").await.expect("B begin share");
    locked(&mut a, "room1", "FOR SHARE", "alice").await;
    locked(&mut b, "room1", "FOR SHARE", "alice").await;
    a.query_drop("COMMIT").await.expect("A commit share");
    b.query_drop("ROLLBACK").await.expect("B rollback share");

    // e) The non-blocking spellings stay rejected (no NOWAIT / SKIP
    //    LOCKED semantics: the engine fails fast on every conflict).
    for sql in [
        "SELECT guest FROM rooms WHERE room = 'room1' FOR UPDATE NOWAIT",
        "SELECT guest FROM rooms WHERE room = 'room1' FOR UPDATE SKIP LOCKED",
    ] {
        let e = server_error(&mut a, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
    }

    // f) Ten hold/release rounds: A takes the row, B is refused, A
    //    commits, B's retry lands. 70 statements, all local.
    let t = scenario_common::perf::now();
    for round in 1..=10 {
        a.query_drop("BEGIN").await.expect("A begin");
        locked(&mut a, "room1", "FOR UPDATE", "alice").await;
        b.query_drop("BEGIN").await.expect("B begin");
        let e = server_error(
            &mut b,
            "SELECT guest FROM rooms WHERE room = 'room1' FOR UPDATE",
        )
        .await;
        assert_eq!(e.code, ER_LOCK_WAIT_TIMEOUT, "round {round}: {e:?}");
        a.query_drop("COMMIT").await.expect("A commit");
        let got = one(
            &mut b,
            "SELECT guest FROM rooms WHERE room = 'room1' FOR UPDATE",
        )
        .await;
        assert_eq!(got, s("alice"), "round {round}: latch released");
        b.query_drop("ROLLBACK").await.expect("B rollback");
    }
    scenario_common::perf::within("lock122/10-rounds", t, Duration::from_secs(2));

    // g) The plain snapshot read never latches: both sessions read the
    //    row with a txn open and nothing conflicts.
    a.query_drop("BEGIN").await.expect("A begin plain");
    b.query_drop("BEGIN").await.expect("B begin plain");
    locked(&mut a, "room1", "", "alice").await;
    locked(&mut b, "room1", "", "alice").await;
    a.query_drop("ROLLBACK").await.expect("A rollback plain");
    b.query_drop("ROLLBACK").await.expect("B rollback plain");

    // h) Autocommit FOR UPDATE latches and releases inside one
    //    statement: the next session's locking read is not refused.
    locked(&mut a, "room2", "FOR UPDATE", "bob").await;
    locked(&mut b, "room2", "FOR UPDATE", "bob").await;

    // i) An unmatched locking read latches nothing (empty match set),
    //    so the same row is still lockable afterwards.
    let got = rows(
        &mut a,
        "SELECT guest FROM rooms WHERE room = 'room9' FOR UPDATE",
    )
    .await;
    assert!(got.is_empty(), "unmatched locking read: {got:?}");
    locked(&mut b, "room1", "FOR UPDATE", "alice").await;

    scenario_common::perf::scenario("lock122", t0);
}
