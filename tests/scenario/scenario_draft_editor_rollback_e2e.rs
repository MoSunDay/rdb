//! Multi-store scenario #107 -- the document draft editor on the SQL
//! plane: START TRANSACTION (the BEGIN alias) opens a private world
//! where a staged insert is visible ONLY to its author until commit,
//! ROLLBACK discards it entirely (nothing ever reaches the store),
//! the COMMIT path lands it for everyone, a second connection's
//! snapshot stays repeatable under a peer's committed writes, and DDL
//! inside a txn is rejected with 1235. One REAL node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run, server_error, ER_NOT_SUPPORTED_YET};
use scenario_common::{front_world, FrontSpec};

#[tokio::test]
async fn draft_editor_stages_rolls_back_and_commits() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("draft107", &spec).await;
    let mut a = connect_root(&front.node).await;
    let mut b = connect_root(&front.node).await;

    ddl(
        &mut a,
        "CREATE TABLE drafts (id BIGINT PRIMARY KEY, body VARCHAR(64) NOT NULL)",
    )
    .await;

    // a) START TRANSACTION opens the private world (BEGIN alias); the
    //    staged draft is author-only.
    run(&mut a, "START TRANSACTION").await;
    run(
        &mut a,
        "INSERT INTO drafts (id, body) VALUES (1, 'v0 draft')",
    )
    .await;
    assert_eq!(
        ints(&mut a, "SELECT COUNT(*) FROM drafts").await,
        vec![1],
        "author sees the stage"
    );
    assert_eq!(
        ints(&mut b, "SELECT COUNT(*) FROM drafts").await,
        vec![0],
        "peer does not"
    );

    // b) ROLLBACK discards the stage: both sides read zero.
    run(&mut a, "ROLLBACK").await;
    assert_eq!(
        ints(&mut a, "SELECT COUNT(*) FROM drafts").await,
        vec![0],
        "author post-rollback"
    );
    assert_eq!(
        ints(&mut b, "SELECT COUNT(*) FROM drafts").await,
        vec![0],
        "nothing reached the store"
    );

    // c) The COMMIT path: restage and land it for everyone.
    run(&mut a, "BEGIN").await;
    run(
        &mut a,
        "INSERT INTO drafts (id, body) VALUES (1, 'v1 draft')",
    )
    .await;
    run(&mut a, "UPDATE drafts SET body = 'v2 draft' WHERE id = 1").await;
    run(&mut a, "COMMIT").await;
    assert_eq!(
        ints(
            &mut b,
            "SELECT COUNT(*) FROM drafts WHERE body = 'v2 draft'"
        )
        .await,
        vec![1],
        "peer sees the committed chain"
    );

    // d) Repeatable read across connections: B opens a snapshot, A
    //    commits a fresh row in autocommit, B still sees ONE row
    //    until its own COMMIT refreshes the snapshot.
    run(&mut b, "START TRANSACTION").await;
    run(
        &mut a,
        "INSERT INTO drafts (id, body) VALUES (2, 'peer draft')",
    )
    .await;
    assert_eq!(
        ints(&mut b, "SELECT COUNT(*) FROM drafts").await,
        vec![1],
        "B's snapshot predates A's commit"
    );
    assert_eq!(
        ints(&mut a, "SELECT COUNT(*) FROM drafts").await,
        vec![2],
        "A sees its own write"
    );
    run(&mut b, "COMMIT").await;
    assert_eq!(
        ints(&mut b, "SELECT COUNT(*) FROM drafts").await,
        vec![2],
        "fresh snapshot after commit"
    );

    // e) DDL inside a txn rejects with 1235 (CREATE TABLE / DROP TABLE /
    //    CREATE INDEX / ALTER RENAME), the txn stays usable for DML, and
    //    the rollback afterwards leaves the world clean.
    run(&mut a, "BEGIN").await;
    for sql in [
        "CREATE TABLE nope (id BIGINT PRIMARY KEY)",
        "DROP TABLE drafts",
        "CREATE INDEX noix ON drafts (id)",
        "ALTER TABLE drafts RENAME TO nope",
    ] {
        let e = server_error(&mut a, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
    }
    run(
        &mut a,
        "INSERT INTO drafts (id, body) VALUES (3, 'staged anyway')",
    )
    .await;
    run(&mut a, "ROLLBACK").await;
    assert_eq!(
        ints(&mut a, "SELECT COUNT(*) FROM drafts").await,
        vec![2],
        "stage discarded after DDL refusals"
    );
}
