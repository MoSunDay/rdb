//! T3 parity: the SAME statement sequence against the host rocks
//! engine (`USE rdb`) and the embedded turso engine (`USE
//! turso_parity`, routed by the `turso_` name prefix on one
//! engine-enabled node). The CRUD trunk must answer identically; the
//! documented deviations are asserted engine-side instead (ledger
//! entries for COMPAT.sql.md):
//!   * DECIMAL(10,2) rides REAL on turso: `10.50` -> `10.5`
//!   * AVG over exact ints: rocks decimal `2.5000` vs turso REAL `2.5`
//!   * UPDATE affected = CHANGED rows (rocks) vs MATCHED (turso)
//!   * multi-row INSERT last-insert id = FIRST generated (rocks) vs
//!     LAST rowid (turso) -- both the OK packet and LAST_INSERT_ID()
//!   * TRUNCATE resets the auto-increment counter (rocks) vs keeps
//!     the watermark (turso)

mod common;

use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;

use common::mysql::{rows, s, spawn_node_mysql_yaml, ER_NO_SUCH_TABLE, PASS};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready};

/// Identical schema + seed on both engines (no UNIQUE key: the
/// no-op/multi-row UPDATE probes below rewrite shared values).
const CREATE: &str = "CREATE TABLE items (\
                      id BIGINT AUTO_INCREMENT PRIMARY KEY, \
                      name VARCHAR(64) NOT NULL, \
                      price DECIMAL(10,2), \
                      weight DOUBLE, \
                      KEY k_name (name))";
const SEED: &str = "INSERT INTO items (name, price, weight) VALUES \
                    ('ada', 10.50, 0.5), ('bob', 2.25, 1.5), ('cyd', 7.75, 2.5)";

/// One statement's OK-packet facts (affected rows + last-insert id).
async fn ok_facts(c: &mut mysql_async::Conn, sql: &str) -> (u64, u64) {
    let result = c.query_iter(sql).await.expect(sql);
    let affected = result.affected_rows();
    let last_id = result.last_insert_id().unwrap_or(0);
    result.drop_result().await.expect("drain result");
    (affected, last_id)
}

/// `sql` must answer `want` rows on BOTH engines.
async fn same(
    r: &mut mysql_async::Conn,
    t: &mut mysql_async::Conn,
    sql: &str,
    want: Vec<Vec<MVal>>,
) {
    assert_eq!(rows(r, sql).await, want.clone(), "rocks: {sql}");
    assert_eq!(rows(t, sql).await, want, "turso: {sql}");
}

#[tokio::test]
async fn turso_parity_trunk() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-parity-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let turso_dir = dir.join("tursodb");
    std::fs::create_dir_all(&turso_dir).unwrap();
    let extra = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let mut node = spawn_node_mysql_yaml(&dir, 0, true, None, &extra);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;

    // One node, two engines: the session database picks the engine.
    let mut rocks = mysql_root_conn(&node, PASS).await;
    common::mysql::ddl(&mut rocks, "CREATE DATABASE turso_parity").await;
    rocks.query_drop("USE rdb").await.expect("use rdb");
    let mut turso = mysql_root_conn(&node, PASS).await;
    turso
        .query_drop("USE turso_parity")
        .await
        .expect("use turso");

    // ---- schema + seed ----
    common::mysql::ddl(&mut rocks, CREATE).await;
    turso.query_drop(CREATE).await.expect("turso create");
    let (r_aff, r_id) = ok_facts(&mut rocks, SEED).await;
    assert_eq!(r_aff, 3, "rocks multi-row insert affected");
    assert_eq!(r_id, 1, "rocks OK-packet last_insert_id = FIRST generated");
    // rocks' LAST_INSERT_ID() reads a process-wide mirror (COMPAT.md
    // deviation) -- probe it BEFORE the turso insert overwrites that
    // mirror with its own watermark.
    assert_eq!(
        rows(&mut rocks, "SELECT LAST_INSERT_ID()").await,
        vec![vec![s("1")]],
        "rocks: first generated id"
    );
    let (t_aff, t_id) = ok_facts(&mut turso, SEED).await;
    assert_eq!(t_aff, 3, "turso multi-row insert affected");
    // ledger: turso's OK packet carries the connection's last rowid.
    assert_eq!(t_id, 3, "turso OK-packet last_insert_id = LAST rowid");
    assert_eq!(
        rows(&mut turso, "SELECT LAST_INSERT_ID()").await,
        vec![vec![s("3")]],
        "turso: last rowid of the statement (ledger)"
    );
    same(
        &mut rocks,
        &mut turso,
        "SELECT id, name FROM items WHERE weight > 1.0 ORDER BY name",
        vec![vec![s("2"), s("bob")], vec![s("3"), s("cyd")]],
    )
    .await;

    // ---- SELECT trunk: order/limit incl. the offset,count swap ----
    same(
        &mut rocks,
        &mut turso,
        "SELECT name, weight FROM items ORDER BY id LIMIT 2",
        vec![vec![s("ada"), s("0.5")], vec![s("bob"), s("1.5")]],
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name FROM items ORDER BY id LIMIT 1, 2",
        vec![vec![s("bob")], vec![s("cyd")]],
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name FROM items ORDER BY id LIMIT 2, 5",
        vec![vec![s("cyd")]],
    )
    .await;

    // ---- aggregates + HAVING ----
    same(
        &mut rocks,
        &mut turso,
        "SELECT COUNT(*), MIN(weight), MAX(weight), SUM(id) FROM items",
        vec![vec![s("3"), s("0.5"), s("2.5"), s("6")]],
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name, COUNT(*) AS cnt FROM items GROUP BY name \
         HAVING cnt = 1 ORDER BY name LIMIT 1",
        vec![vec![s("ada"), s("1")]],
    )
    .await;

    // ---- scalar-function allowlist (shared MySQL face) ----
    same(
        &mut rocks,
        &mut turso,
        "SELECT LENGTH('héllo'), SUBSTRING('abcdef', 2, 3), CONCAT('x', 'y'), \
         UPPER('aB'), ABS(-5), MOD(7, 3), COALESCE(NULL, 'd'), IFNULL(NULL, 7)",
        vec![vec![
            s("6"),
            s("bcd"),
            s("xy"),
            s("AB"),
            s("5"),
            s("1"),
            s("d"),
            s("7"),
        ]],
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT ROUND(2.5), FLOOR(1.9), CEIL(1.1)",
        vec![vec![s("3"), s("1"), s("2")]],
    )
    .await;

    // ---- ledger: DECIMAL scale rides REAL on turso ----
    assert_eq!(
        rows(&mut rocks, "SELECT price FROM items ORDER BY id").await,
        vec![vec![s("10.50")], vec![s("2.25")], vec![s("7.75")]],
        "rocks: exact DECIMAL(10,2) text"
    );
    assert_eq!(
        rows(&mut turso, "SELECT price FROM items ORDER BY id").await,
        vec![vec![s("10.5")], vec![s("2.25")], vec![s("7.75")]],
        "turso: DECIMAL rides REAL (ledger)"
    );

    // ---- UPDATE: changing update, identical facts + visible effect ----
    let upd = "UPDATE items SET weight = 9.5 WHERE id <= 2";
    let (r_aff, _) = ok_facts(&mut rocks, upd).await;
    let (t_aff, _) = ok_facts(&mut turso, upd).await;
    assert_eq!((r_aff, t_aff), (2, 2), "changed update affected");
    same(
        &mut rocks,
        &mut turso,
        "SELECT weight FROM items ORDER BY id",
        vec![vec![s("9.5")], vec![s("9.5")], vec![s("2.5")]],
    )
    .await;

    // ---- ledger: no-op UPDATE affected (changed vs matched) ----
    let noop = "UPDATE items SET weight = 9.5 WHERE id = 1";
    let (r_aff, _) = ok_facts(&mut rocks, noop).await;
    let (t_aff, _) = ok_facts(&mut turso, noop).await;
    assert_eq!(r_aff, 0, "rocks counts CHANGED rows");
    assert_eq!(t_aff, 1, "turso counts MATCHED rows (ledger)");

    // ---- DELETE: identical facts ----
    let del = "DELETE FROM items WHERE id = 1";
    let (r_aff, _) = ok_facts(&mut rocks, del).await;
    let (t_aff, _) = ok_facts(&mut turso, del).await;
    assert_eq!((r_aff, t_aff), (1, 1), "delete affected");
    same(
        &mut rocks,
        &mut turso,
        "SELECT COUNT(*) FROM items",
        vec![vec![s("2")]],
    )
    .await;

    // ---- ledger: AVG over exact ints (decimal scale+4 vs REAL) ----
    assert_eq!(
        rows(&mut rocks, "SELECT AVG(id) FROM items").await,
        vec![vec![s("2.5000")]],
        "rocks: decimal division at scale+4"
    );
    assert_eq!(
        rows(&mut turso, "SELECT AVG(id) FROM items").await,
        vec![vec![s("2.5")]],
        "turso: REAL average (ledger)"
    );

    // ---- ledger: TRUNCATE auto-increment counter ----
    rocks
        .query_drop("TRUNCATE items")
        .await
        .expect("rocks truncate");
    turso
        .query_drop("TRUNCATE items")
        .await
        .expect("turso truncate");
    let post = "INSERT INTO items (name, price, weight) VALUES ('post', 1.0, 0.25)";
    let _ = ok_facts(&mut rocks, post).await;
    let _ = ok_facts(&mut turso, post).await;
    assert_eq!(
        rows(&mut rocks, "SELECT id FROM items").await,
        vec![vec![s("1")]],
        "rocks: counter reset by TRUNCATE"
    );
    assert_eq!(
        rows(&mut turso, "SELECT id FROM items").await,
        vec![vec![s("4")]],
        "turso: watermark kept across TRUNCATE (ledger)"
    );

    // ---- DROP: identical effect + errno shape ----
    rocks
        .query_drop("DROP TABLE items")
        .await
        .expect("rocks drop");
    turso
        .query_drop("DROP TABLE items")
        .await
        .expect("turso drop");
    let e = mysql_server_error(&mut rocks, "SELECT * FROM items").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "rocks: {e:?}");
    let e = mysql_server_error(&mut turso, "SELECT * FROM items").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "turso: {e:?}");

    node.kill_now();
}
