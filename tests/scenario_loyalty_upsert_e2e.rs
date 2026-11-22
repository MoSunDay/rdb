//! Multi-store scenario #100 -- loyalty daily-points accrual on the
//! SQL plane through the MySQL upsert trio: the SAME
//! INSERT ... ON DUPLICATE KEY UPDATE statement inserts a new member
//! (affected 1), accrues on conflict with VALUES() arithmetic reading
//! the incoming row (affected 2), and reports 0 when the update lands
//! on identical values; REPLACE INTO swaps the row wholesale
//! (affected = deleted + 1); INSERT ... SELECT self-snapshots the
//! ledger with suffixed ids. One REAL node, MySQL front.

mod common;
mod scenario_common;

use common::mysql::{aff, connect_root, ddl, rows};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply.iter().map(|r| r.iter().map(cell).collect()).collect()
}

const ACCRUE: &str = "INSERT INTO members (id, points, streak) VALUES ('u1', 10, 1) \
         ON DUPLICATE KEY UPDATE points = points + VALUES(points), streak = streak + 1";

#[tokio::test]
async fn loyalty_accrual_pins_the_upsert_affected_semantics() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("loyal100", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The ledger.
    ddl(&mut c, "CREATE TABLE members (id VARCHAR(24) PRIMARY KEY, points INT NOT NULL, streak INT NOT NULL)").await;

    // b) The accrual statement on a fresh id takes the INSERT branch.
    assert_eq!(aff(&mut c, ACCRUE).await, 1, "odku insert branch");
    let first = rows(&mut c, "SELECT points, streak FROM members WHERE id = 'u1'").await;
    assert_eq!(grid(&first), vec![vec!["10", "1"]], "seeded");

    // c) Replaying it takes the UPDATE branch: VALUES(points) reads the
    //    INCOMING row (10) and stacks on the stored one.
    assert_eq!(aff(&mut c, ACCRUE).await, 2, "odku changed update");
    let stacked = rows(&mut c, "SELECT points, streak FROM members WHERE id = 'u1'").await;
    assert_eq!(grid(&stacked), vec![vec!["20", "2"]], "values() arithmetic");

    // d) An ODKU whose assignments land on identical values writes
    //    nothing: affected 0 (the row compare precedes the write).
    assert_eq!(
        aff(&mut c, "INSERT INTO members (id, points, streak) VALUES ('u1', 10, 1) \
             ON DUPLICATE KEY UPDATE points = 20, streak = 2")
            .await,
        0,
        "odku identical update"
    );
    let unchanged = rows(&mut c, "SELECT points, streak FROM members WHERE id = 'u1'").await;
    assert_eq!(grid(&unchanged), vec![vec!["20", "2"]], "no rewrite");

    // e) REPLACE INTO an existing key: delete + reinsert, affected 2.
    assert_eq!(
        aff(&mut c, "REPLACE INTO members (id, points, streak) VALUES ('u1', 50, 3)").await,
        2,
        "replace existing = deleted + 1"
    );
    assert_eq!(
        aff(&mut c, "REPLACE INTO members (id, points, streak) VALUES ('u2', 7, 1)").await,
        1,
        "replace fresh key"
    );
    let board = rows(&mut c, "SELECT id, points, streak FROM members ORDER BY id").await;
    assert_eq!(
        grid(&board),
        vec![vec!["u1", "50", "3"], vec!["u2", "7", "1"]],
        "replaced wholesale"
    );

    // f) INSERT ... SELECT self-snapshot: each member forks under a
    //    suffixed id, carrying points and streak verbatim.
    assert_eq!(
        aff(&mut c, "INSERT INTO members (id, points, streak) \
             SELECT CONCAT(id, '-snap'), points, streak FROM members").await,
        2,
        "insert-select snapshot"
    );
    let snaps = rows(
        &mut c,
        "SELECT id, points FROM members WHERE id LIKE '%-snap' ORDER BY id",
    )
    .await;
    assert_eq!(
        grid(&snaps),
        vec![vec!["u1-snap", "50"], vec!["u2-snap", "7"]],
        "snapshot copies"
    );
    // The sources survive the fork untouched.
    let srcs = rows(
        &mut c,
        "SELECT COUNT(*) FROM members WHERE id NOT LIKE '%-snap'",
    )
    .await;
    assert_eq!(grid(&srcs), vec![vec!["2"]], "sources intact");
}
