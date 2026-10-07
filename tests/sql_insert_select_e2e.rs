//! M2 DML-conflict e2e (part 2, single node, see
//! `plans/2026-10-06-mysql-gap/m2-dml-conflicts.md`): `INSERT ...
//! SELECT` with pre-statement snapshot semantics (same-table reads),
//! cross-table sources with WHERE, the aggregating upsert
//! (SELECT+GROUP BY feeding ODKU), `INSERT ... SET` normalization, the
//! AUTO_INCREMENT id burn on the ODKU update branch (MySQL parity) and
//! the negative matrix (stray `VALUES()`, unknown assignment column,
//! columnar rejects, plain INSERT's unique 1062). ODKU/REPLACE core
//! lives in `tests/sql_upsert_e2e.rs`; cluster vetoes in
//! `tests/sql_upsert_cluster_e2e.rs`.

mod common;

use common::mysql::{aff, ddl, ints, pairs, server_error, world};
use common::mysql::{ER_BAD_FIELD_ERROR, ER_DUP_ENTRY, ER_NOT_SUPPORTED_YET, ER_PARSE_ERROR};

#[tokio::test]
async fn insert_select_snapshot_cross_table_and_set() {
    let (mut node, mut c) = world("isel").await;
    ddl(
        &mut c,
        "CREATE TABLE sd (id BIGINT PRIMARY KEY, v VARCHAR(8) NULL)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE agg (k BIGINT PRIMARY KEY, total BIGINT NOT NULL)",
    )
    .await;

    // same-table INSERT..SELECT reads the PRE-statement snapshot: the
    // doubling below would diverge (or loop) if reads saw the writes
    aff(
        &mut c,
        "INSERT INTO sd (id, v) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
    )
    .await;
    assert_eq!(
        aff(&mut c, "INSERT INTO sd SELECT id + 100, v FROM sd").await,
        3
    );
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM sd ORDER BY id").await,
        vec![
            (1, "a".to_string()),
            (2, "b".to_string()),
            (3, "c".to_string()),
            (101, "a".to_string()),
            (102, "b".to_string()),
            (103, "c".to_string()),
        ]
    );
    // a second doubling confirms the snapshot boundary (101..103 are
    // plain-INSERT pk upserts over themselves, 201..203 are fresh)
    assert_eq!(
        aff(&mut c, "INSERT INTO sd SELECT id + 100, v FROM sd").await,
        6
    );
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM sd").await, vec![9]);

    // cross-table with WHERE (rid is the pk, so k MAY repeat: a k-pk
    // would collapse the duplicate through plain INSERT's silent
    // pk upsert)
    ddl(
        &mut c,
        "CREATE TABLE src (rid BIGINT PRIMARY KEY, k BIGINT NOT NULL, v BIGINT NOT NULL)",
    )
    .await;
    aff(
        &mut c,
        "INSERT INTO src (rid, k, v) VALUES (1, 1, 5), (2, 2, 50), (3, 3, 7), (4, 4, 70), (5, 2, 5)",
    )
    .await;
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO agg (k, total) SELECT k, v FROM src WHERE v > 10"
        )
        .await,
        2
    );
    assert_eq!(
        pairs(&mut c, "SELECT k, total FROM agg ORDER BY k").await,
        vec![(2, "50".to_string()), (4, "70".to_string())]
    );

    // aggregating upsert: SUM per key lands through ODKU, adding onto
    // whatever the target already holds (new -> 1, changed -> 2 each)
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO agg (k, total) SELECT k, SUM(v) FROM src GROUP BY k \
             ON DUPLICATE KEY UPDATE total = total + VALUES(total)"
        )
        .await,
        6
    );
    assert_eq!(
        pairs(&mut c, "SELECT k, total FROM agg ORDER BY k").await,
        vec![
            (1, "5".to_string()),
            (2, "105".to_string()), // 50 staged + SUM(50+5)
            (3, "7".to_string()),
            (4, "140".to_string()), // 70 staged + SUM(70)
        ]
    );
    // running it again re-adds every SUM once more (all four rows
    // conflict with changed values: 8 = 4 * 2)
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO agg (k, total) SELECT k, SUM(v) FROM src GROUP BY k \
             ON DUPLICATE KEY UPDATE total = total + VALUES(total)"
        )
        .await,
        8
    );
    assert_eq!(
        pairs(&mut c, "SELECT k, total FROM agg WHERE k = 2").await,
        vec![(2, "160".to_string())] // 105 + SUM(55) again
    );

    // INSERT ... SET normalizes to a named single-row VALUES list and
    // rides every conflict path the explicit form does
    assert_eq!(
        aff(&mut c, "INSERT INTO agg SET k = 90, total = 7").await,
        1
    );
    assert_eq!(
        aff(&mut c, "INSERT INTO agg SET k = 91, total = 3 + 4").await,
        1
    );
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO agg SET k = 90, total = 1 ON DUPLICATE KEY UPDATE total = total + VALUES(total)"
        )
        .await,
        2
    );
    assert_eq!(
        pairs(&mut c, "SELECT k, total FROM agg WHERE k >= 90 ORDER BY k").await,
        vec![(90, "8".to_string()), (91, "7".to_string())]
    );
    node.kill_now();
}

#[tokio::test]
async fn auto_increment_burns_on_odku_update_branch() {
    let (mut node, mut c) = world("aiburn").await;
    ddl(
        &mut c,
        "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(8) NULL)",
    )
    .await;
    ddl(&mut c, "CREATE UNIQUE INDEX ai_v ON ai (v)").await;

    assert_eq!(
        aff(&mut c, "INSERT INTO ai (id, v) VALUES (NULL, 'a')").await,
        1
    );
    assert_eq!(ints(&mut c, "SELECT id FROM ai").await, vec![1]);
    // the update branch still burns its pre-allocated id (MySQL gap
    // parity): id 2 is spent even though no row wears it
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ai (v) VALUES ('a') ON DUPLICATE KEY UPDATE v = 'b'"
        )
        .await,
        2
    );
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM ai").await,
        vec![(1, "b".to_string())]
    );
    // the NEXT fresh row lands strictly above 2 (the allocator hands
    // out reserve batches, see exec/sequence.rs; what matters is that
    // the burned id never comes back)
    assert_eq!(aff(&mut c, "INSERT INTO ai (v) VALUES ('c')").await, 1);
    let ids = ints(&mut c, "SELECT id FROM ai ORDER BY id").await;
    assert_eq!(ids[0], 1);
    assert!(
        ids[1] > 2,
        "id 2 was burned by the ODKU update branch: {ids:?}"
    );
    node.kill_now();
}

#[tokio::test]
async fn negative_matrix() {
    let (mut node, mut c) = world("neg").await;
    ddl(
        &mut c,
        "CREATE TABLE neg (id BIGINT PRIMARY KEY, v VARCHAR(8) NULL)",
    )
    .await;
    aff(&mut c, "INSERT INTO neg (id, v) VALUES (1, 'a')").await;
    ddl(
        &mut c,
        "CREATE TABLE cz (id BIGINT PRIMARY KEY, v VARCHAR(8) NULL) ENGINE=columnar",
    )
    .await;

    // columnar tables are append-only: all three conflict/SELECT
    // shapes reject loudly with 1235 naming the engine
    for sql in [
        "INSERT INTO cz (id, v) VALUES (1, 'a') ON DUPLICATE KEY UPDATE v = VALUES(v)",
        "REPLACE INTO cz (id, v) VALUES (1, 'a')",
        "INSERT INTO cz SELECT id, v FROM neg",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e}");
        assert!(e.message.contains("columnar"), "{sql}: {e}");
    }

    // VALUES(col) is only legal inside ODKU assignments: inside an
    // INSERT tuple it is a parse error...
    let e = server_error(&mut c, "INSERT INTO neg (id, v) VALUES (2, VALUES(v))").await;
    assert_eq!(e.code, ER_PARSE_ERROR, "{e}");
    assert!(e.message.contains("ON DUPLICATE KEY UPDATE"), "{e}");
    // ...and everywhere else it degenerates to an unknown function
    for sql in [
        "UPDATE neg SET v = VALUES(v) WHERE id = 1",
        "DELETE FROM neg WHERE v = VALUES(v)",
        "SELECT VALUES(v) FROM neg",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e}");
        assert!(e.message.contains("unknown function"), "{sql}: {e}");
    }

    // unknown ODKU assignment column -> 1054
    let e = server_error(
        &mut c,
        "INSERT INTO neg (id, v) VALUES (1, 'a') ON DUPLICATE KEY UPDATE nope = 1",
    )
    .await;
    assert_eq!(e.code, ER_BAD_FIELD_ERROR, "{e}");
    assert!(e.message.contains("unknown column"), "{e}");
    // REPLACE ... ON DUPLICATE KEY UPDATE is a parse error (MySQL too)
    let e = server_error(
        &mut c,
        "REPLACE INTO neg (id) VALUES (1) ON DUPLICATE KEY UPDATE v = 'x'",
    )
    .await;
    assert_eq!(e.code, ER_PARSE_ERROR, "{e}");

    // plain INSERT keeps its pre-M2 shape: pk duplicates silently
    // upsert (intentional deviation), unique hits stay 1062
    ddl(&mut c, "CREATE UNIQUE INDEX neg_v ON neg (v)").await;
    aff(&mut c, "INSERT INTO neg (id, v) VALUES (1, 'a2')").await;
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM neg").await,
        vec![(1, "a2".to_string())]
    );
    let e = server_error(&mut c, "INSERT INTO neg (id, v) VALUES (9, 'a2')").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e}");
    node.kill_now();
}
