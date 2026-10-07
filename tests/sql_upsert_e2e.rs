//! M2 DML-conflict e2e (part 1, single node) over a REAL rdb
//! MySQL-protocol process (see `plans/2026-10-06-mysql-gap/
//! m2-dml-conflicts.md`): `INSERT ... ON DUPLICATE KEY UPDATE` and
//! `REPLACE INTO` -- the MySQL 1/2/0 affected-rows matrix across pk-
//! and unique-conflict paths, `VALUES(col)` references with
//! arithmetic, unassigned-column retention, pk-moving ODKU with index
//! migration, multi-unique REPLACE, txn rollback and kill -9 restart
//! durability. INSERT..SELECT/SET and the negative matrix live in
//! `tests/sql_insert_select_e2e.rs`; cluster vetoes in
//! `tests/sql_upsert_cluster_e2e.rs`.

mod common;

use common::mysql::{aff, ddl, ints, pairs, server_error, triples, world};
use common::mysql::{ER_DUP_ENTRY, PASS};
use common::{mysql_root_conn, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;

#[tokio::test]
async fn odku_affected_rows_matrix() {
    let (mut node, mut c) = world("aff").await;
    ddl(
        &mut c,
        "CREATE TABLE ar (id BIGINT PRIMARY KEY, v VARCHAR(32) NULL, w VARCHAR(32) NULL)",
    )
    .await;
    ddl(&mut c, "CREATE UNIQUE INDEX ar_v ON ar (v)").await;

    // fresh insert -> 1
    assert_eq!(
        aff(&mut c, "INSERT INTO ar (id, v) VALUES (1, 'a')").await,
        1
    );
    // pk conflict, value changes -> 2
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ar (id, v) VALUES (1, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        2
    );
    // pk conflict, identical values -> 0
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ar (id, v) VALUES (1, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        0
    );
    // unique(v) conflict on a fresh pk, value changes -> 2 (the owner
    // row takes the update branch; the incoming row never lands)
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ar (id, v) VALUES (2, 'b') ON DUPLICATE KEY UPDATE v = 'c'"
        )
        .await,
        2
    );
    // unique(v) conflict, identical values -> 0
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ar (id, v) VALUES (3, 'c') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        0
    );
    // multi-row statement with mixed outcomes: [new -> 1,
    // pk-conflict-changed -> 2, unique-conflict-identical -> 0] = 3
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ar (id, v) VALUES (4, 'd'), (1, 'e'), (5, 'e') \
             ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        3
    );
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM ar ORDER BY id").await,
        vec![(1, "e".to_string()), (4, "d".to_string())],
        "the identical-conflict row never landed, the changed row did"
    );
    node.kill_now();
}

#[tokio::test]
async fn odku_values_refs_retention_and_pk_change() {
    let (mut node, mut c) = world("vals").await;
    ddl(
        &mut c,
        "CREATE TABLE cv (id BIGINT PRIMARY KEY, c BIGINT NOT NULL, tag VARCHAR(16) NULL)",
    )
    .await;
    ddl(&mut c, "CREATE UNIQUE INDEX cv_tag ON cv (tag)").await;
    aff(&mut c, "INSERT INTO cv (id, c, tag) VALUES (1, 10, 'x')").await;

    // plain column refs read the EXISTING row, VALUES(col) the
    // INCOMING one: c = 10 + 5
    aff(
        &mut c,
        "INSERT INTO cv (id, c, tag) VALUES (1, 5, 'x') ON DUPLICATE KEY UPDATE c = c + VALUES(c)",
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT c FROM cv WHERE id = 1").await,
        vec![15]
    );

    // unassigned columns keep the EXISTING row's value (tag stays 'x',
    // never the incoming 'zz')
    aff(
        &mut c,
        "INSERT INTO cv (id, c, tag) VALUES (1, 7, 'zz') ON DUPLICATE KEY UPDATE c = VALUES(c)",
    )
    .await;
    assert_eq!(
        triples(&mut c, "SELECT id, c, tag FROM cv").await,
        vec![(1, "7".to_string(), "x".to_string())]
    );

    // PK-changing ODKU: row 1 migrates to pk 2 in the same write; the
    // unique tag entry follows the row, so the index answers with the
    // NEW pk
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO cv (id, c, tag) VALUES (1, 99, 'q') \
             ON DUPLICATE KEY UPDATE id = 2, c = VALUES(c)"
        )
        .await,
        2
    );
    assert_eq!(
        triples(&mut c, "SELECT id, c, tag FROM cv").await,
        vec![(2, "99".to_string(), "x".to_string())]
    );
    assert_eq!(
        ints(&mut c, "SELECT id FROM cv WHERE tag = 'x'").await,
        vec![2]
    );
    // the vacated pk is reusable, and 'q' (the never-landed incoming
    // tag) left no stale unique entry behind
    aff(&mut c, "INSERT INTO cv (id, c, tag) VALUES (1, 1, 'y')").await;
    aff(&mut c, "INSERT INTO cv (id, c, tag) VALUES (3, 3, 'q')").await;
    assert_eq!(
        ints(&mut c, "SELECT id FROM cv ORDER BY id").await,
        vec![1, 2, 3]
    );
    node.kill_now();
}

#[tokio::test]
async fn replace_deletes_every_conflict_and_counts() {
    let (mut node, mut c) = world("repl").await;
    ddl(
        &mut c,
        "CREATE TABLE rp (id BIGINT PRIMARY KEY, v VARCHAR(16) NULL, w VARCHAR(16) NULL)",
    )
    .await;
    ddl(&mut c, "CREATE UNIQUE INDEX rp_v ON rp (v)").await;
    ddl(&mut c, "CREATE UNIQUE INDEX rp_w ON rp (w)").await;
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO rp (id, v, w) VALUES (1, 'a', 'p'), (2, 'b', 'q')"
        )
        .await,
        2
    );

    // one row hitting BOTH unique indexes (v with row 1, w with row 2):
    // 2 deleted + 1 inserted = 3
    assert_eq!(
        aff(&mut c, "REPLACE INTO rp (id, v, w) VALUES (3, 'a', 'q')").await,
        3
    );
    assert_eq!(
        triples(&mut c, "SELECT id, v, w FROM rp").await,
        vec![(3, "a".to_string(), "q".to_string())]
    );
    // pk-only conflict: 1 delete + 1 insert = 2
    assert_eq!(
        aff(&mut c, "REPLACE INTO rp (id, v, w) VALUES (3, 'z', 'z')").await,
        2
    );
    // no conflict at all: 1
    assert_eq!(
        aff(&mut c, "REPLACE INTO rp (id, v, w) VALUES (4, 'k', 'k')").await,
        1
    );
    // multi-row REPLACE: pk hit (2) then a clean insert (1)
    assert_eq!(
        aff(
            &mut c,
            "REPLACE INTO rp (id, v, w) VALUES (3, 'm', 'n'), (9, 'a', 'q')"
        )
        .await,
        3
    );
    assert_eq!(
        triples(&mut c, "SELECT id, v, w FROM rp ORDER BY id").await,
        vec![
            (3, "m".to_string(), "n".to_string()),
            (4, "k".to_string(), "k".to_string()),
            (9, "a".to_string(), "q".to_string()),
        ]
    );
    // both unique indexes still answer (entries moved with the churn)
    assert_eq!(
        ints(&mut c, "SELECT id FROM rp WHERE v = 'a'").await,
        vec![9]
    );
    assert_eq!(
        ints(&mut c, "SELECT id FROM rp WHERE w = 'q'").await,
        vec![9]
    );
    // and still enforce: a plain INSERT onto a taken value is 1062
    let e = server_error(&mut c, "INSERT INTO rp (id, v, w) VALUES (5, 'a', NULL)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e}");
    node.kill_now();
}

#[tokio::test]
async fn txn_rollback_restores_odku_and_replace() {
    let (mut node, mut c) = world("txn").await;
    ddl(
        &mut c,
        "CREATE TABLE tb (id BIGINT PRIMARY KEY, v VARCHAR(16) NULL)",
    )
    .await;
    assert_eq!(
        aff(&mut c, "INSERT INTO tb (id, v) VALUES (1, 'a'), (2, 'b')").await,
        2
    );

    c.query_drop("BEGIN").await.expect("begin");
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO tb (id, v) VALUES (1, 'a2') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        2
    );
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO tb (id, v) VALUES (3, 'c') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        1
    );
    assert_eq!(
        aff(&mut c, "REPLACE INTO tb (id, v) VALUES (2, 'b2')").await,
        2
    );
    // INSERT..SELECT inside the txn reads the txn's merged view (the
    // three staged rows overlay the two committed ones by pk)
    assert_eq!(
        aff(&mut c, "INSERT INTO tb (id, v) SELECT id + 10, v FROM tb").await,
        3
    );
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM tb ORDER BY id").await,
        vec![
            (1, "a2".to_string()),
            (2, "b2".to_string()),
            (3, "c".to_string()),
            (11, "a2".to_string()),
            (12, "b2".to_string()),
            (13, "c".to_string()),
        ],
        "staged writes must be visible inside the txn"
    );
    c.query_drop("ROLLBACK").await.expect("rollback");

    // pre-txn truth survives: row count AND values
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM tb ORDER BY id").await,
        vec![(1, "a".to_string()), (2, "b".to_string())]
    );
    node.kill_now();
}

#[tokio::test]
async fn kill9_restart_keeps_conflict_writes_and_unique_index() {
    let (mut node, mut c) = world("restart").await;
    ddl(
        &mut c,
        "CREATE TABLE ur (id BIGINT PRIMARY KEY, v VARCHAR(16) NULL)",
    )
    .await;
    ddl(&mut c, "CREATE UNIQUE INDEX ur_v ON ur (v)").await;

    assert_eq!(
        aff(&mut c, "INSERT INTO ur (id, v) VALUES (1, 'a')").await,
        1
    );
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ur (id, v) VALUES (1, 'b') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        2
    );
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ur (id, v) VALUES (2, 'c') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        1
    );
    assert_eq!(
        aff(&mut c, "REPLACE INTO ur (id, v) VALUES (1, 'd')").await,
        2
    );
    // unique(v) hit: row 2 dies, row 3 takes 'c'
    assert_eq!(
        aff(&mut c, "REPLACE INTO ur (id, v) VALUES (3, 'c')").await,
        2
    );
    // same-table INSERT..SELECT (fresh values keep the unique index
    // happy: 'r' || v never collides with v)
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ur (id, v) SELECT id + 100, CONCAT('r', v) FROM ur"
        )
        .await,
        2
    );
    let before = pairs(&mut c, "SELECT id, v FROM ur ORDER BY id").await;
    assert_eq!(
        before,
        vec![
            (1, "d".to_string()),
            (3, "c".to_string()),
            (101, "rd".to_string()),
            (103, "rc".to_string()),
        ]
    );

    // kill -9 + restart on the same data dir
    node.kill_now();
    node.respawn();
    wait_resp_ready(&mut node, 30).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM ur ORDER BY id").await,
        before,
        "conflict-path writes lost across kill -9 restart\n{}",
        node.ctx()
    );
    // the unique index survived: taken values still reject with 1062
    let e = server_error(&mut c, "INSERT INTO ur (id, v) VALUES (9, 'rc')").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e}");
    assert_eq!(
        ints(&mut c, "SELECT id FROM ur WHERE v = 'rc'").await,
        vec![103]
    );
    // and the conflict paths still decide correctly after the restart
    assert_eq!(
        aff(
            &mut c,
            "INSERT INTO ur (id, v) VALUES (1, 'd') ON DUPLICATE KEY UPDATE v = VALUES(v)"
        )
        .await,
        0
    );
    assert_eq!(
        aff(&mut c, "REPLACE INTO ur (id, v) VALUES (1, 'e')").await,
        2
    );
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM ur WHERE id = 1").await,
        vec![(1, "e".to_string())]
    );
    node.kill_now();
}

#[tokio::test]
async fn freed_unique_values_and_pk_move_collisions() {
    let (mut node, mut c) = world("free").await;
    ddl(
        &mut c,
        "CREATE TABLE fr (id BIGINT PRIMARY KEY, v VARCHAR(16) NULL)",
    )
    .await;
    ddl(&mut c, "CREATE UNIQUE INDEX fr_v ON fr (v)").await;
    assert_eq!(
        aff(&mut c, "INSERT INTO fr (id, v) VALUES (1, 'x')").await,
        1
    );
    // (1,'y') replaces row 1 and frees 'x'; (2,'x') then conflicts
    // with NOTHING: affected 2 + 1 = 3 and both rows survive (a stale
    // unique snapshot would still blame row 1: affected 4, one row
    // silently lost).
    assert_eq!(
        aff(&mut c, "REPLACE INTO fr (id, v) VALUES (1, 'y'), (2, 'x')").await,
        3
    );
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM fr ORDER BY id").await,
        vec![(1, "y".to_string()), (2, "x".to_string())]
    );
    assert_eq!(
        aff(&mut c, "INSERT INTO fr (id, v) VALUES (3, 'z'), (4, 'q')").await,
        2
    );
    // ODKU unique-conflict whose assignment moves the pk onto live
    // row 1: the server rejects with 1062 and nothing lands.
    let e = server_error(
        &mut c,
        "INSERT INTO fr (id, v) VALUES (5, 'q') ON DUPLICATE KEY UPDATE id = 1",
    )
    .await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e}");
    assert_eq!(
        pairs(&mut c, "SELECT id, v FROM fr ORDER BY id").await,
        vec![
            (1, "y".to_string()),
            (2, "x".to_string()),
            (3, "z".to_string()),
            (4, "q".to_string()),
        ]
    );
    // plain UPDATE merging two live rows is the same 1062 disease
    let e = server_error(&mut c, "UPDATE fr SET id = 2 WHERE id = 3").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e}");
    node.kill_now();
}
