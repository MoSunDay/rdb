//! T3 turso-engine CRUD trunk over one real rdb process (see
//! plans/2026-11-09-turso-engine): typed CREATE TABLE (+pk
//! AUTO_INCREMENT + NOT NULL + inline secondary index), multi-row
//! INSERT with engine-side id allocation and LAST_INSERT_ID, the
//! SELECT trunk (WHERE / ORDER BY / LIMIT incl. the MySQL
//! offset,limit swap / GROUP BY aggregates), UPDATE and DELETE with
//! affected-rows checks, TRUNCATE/DROP, and the 1235 rejection shape
//! for everything outside the allowlist.

use crate::common;

use mysql_async::prelude::Queryable;

use common::mysql::{
    rows, s, spawn_node_mysql_yaml, ER_DUP_ENTRY, ER_NOT_SUPPORTED_YET, ER_NO_SUCH_TABLE, PASS,
};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready};

/// One statement's OK-packet facts (affected rows + last-insert id).
async fn ok_facts(c: &mut mysql_async::Conn, sql: &str) -> (u64, u64) {
    let result = c.query_iter(sql).await.expect(sql);
    let affected = result.affected_rows();
    let last_id = result.last_insert_id().unwrap_or(0);
    result.drop_result().await.expect("drain result");
    (affected, last_id)
}

#[tokio::test]
async fn turso_crud_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-crud-e2e-{}", std::process::id()));
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
    let mut c = mysql_root_conn(&node, PASS).await;

    // Library + session pin. (DEFAULT column values are dropped by the
    // shared parser before either engine sees them -- not assertable
    // here; ledger entry.)
    common::mysql::ddl(&mut c, "CREATE DATABASE turso_crud").await;
    c.query_drop("USE turso_crud")
        .await
        .expect("use turso_crud");

    // (a) CREATE TABLE: typed columns, AUTO_INCREMENT int pk, NOT
    // NULL, DOUBLE, DECIMAL->REAL (ledger), inline secondary +
    // unique indexes (rendered as CREATE INDEX after the table).
    c.query_drop(
        "CREATE TABLE items (\
         id BIGINT AUTO_INCREMENT PRIMARY KEY, \
         name VARCHAR(64) NOT NULL, \
         price DECIMAL(10,2), \
         weight DOUBLE, \
         KEY k_name (name), \
         UNIQUE KEY u_weight (weight))",
    )
    .await
    .expect("create items");

    // (b) Multi-row INSERT, AUTO_INCREMENT column omitted: SQLite's
    // INTEGER PRIMARY KEY AUTOINCREMENT allocates 1..3; the OK packet
    // carries 3 affected rows and the connection's last-insert id.
    let (aff, last_id) = ok_facts(
        &mut c,
        "INSERT INTO items (name, price, weight) VALUES \
         ('ada', 10.50, 0.5), ('bob', 2.25, 1.5), ('cyd', 7.75, 2.5)",
    )
    .await;
    assert_eq!(aff, 3, "multi-row insert affected");
    assert_eq!(last_id, 3, "OK-packet last_insert_id (engine rowid)");
    // LAST_INSERT_ID() renders to last_insert_rowid(): the pinned
    // connection's watermark = LAST of the statement, where the host
    // engine reports the FIRST (ledger deviation).
    assert_eq!(
        rows(&mut c, "SELECT LAST_INSERT_ID()").await,
        vec![vec![s("3")]]
    );

    // (c) SELECT trunk.
    assert_eq!(
        rows(
            &mut c,
            "SELECT id, name FROM items WHERE weight > 1.0 ORDER BY name"
        )
        .await,
        vec![vec![s("2"), s("bob")], vec![s("3"), s("cyd")]],
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT name AS n, weight FROM items ORDER BY id LIMIT 2"
        )
        .await,
        vec![vec![s("ada"), s("0.5")], vec![s("bob"), s("1.5")]]
    );
    // MySQL `LIMIT offset, count` (skip 1, take 2) -- the parser
    // normalized the argument swap; SQLite spells LIMIT 2 OFFSET 1.
    assert_eq!(
        common::mysql::col(&mut c, "SELECT name FROM items ORDER BY id LIMIT 1, 2").await,
        vec![s("bob"), s("cyd")]
    );
    // ledger: DECIMAL(10,2) rides REAL on turso -- '7.75' keeps its
    // spelling, but scale-carrying values (10.50) drop trailing zeros
    // vs the host engine's exact '10.50' (parity suite asserts this).
    assert_eq!(
        rows(&mut c, "SELECT price FROM items ORDER BY id").await,
        vec![vec![s("10.5")], vec![s("2.25")], vec![s("7.75")]]
    );
    // Aggregates with GROUP BY + HAVING.
    assert_eq!(
        rows(
            &mut c,
            "SELECT COUNT(*), MIN(weight), MAX(weight), SUM(id) FROM items"
        )
        .await,
        vec![vec![s("3"), s("0.5"), s("2.5"), s("6")]]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT name, COUNT(*) AS cnt FROM items GROUP BY name HAVING cnt = 1 \
             ORDER BY name LIMIT 1"
        )
        .await,
        vec![vec![s("ada"), s("1")]]
    );
    // Scalar-function allowlist (turso renderings of the MySQL face).
    assert_eq!(
        rows(
            &mut c,
            "SELECT LENGTH('héllo'), UPPER('aB'), SUBSTRING('abcdef', 2, 3), \
             CONCAT('x', 'y'), ABS(-5), ROUND(2.5), FLOOR(1.9), CEIL(1.1), \
             MOD(7, 3), COALESCE(NULL, 'd'), IFNULL(NULL, 7)"
        )
        .await,
        vec![vec![
            s("6"), // LENGTH is MySQL BYTE length -> CAST AS BLOB
            s("AB"),
            s("bcd"),
            s("xy"),
            s("5"),
            s("3"), // ROUND/FLOOR/CEIL answer integers on turso too
            s("1"),
            s("2"),
            s("1"),
            s("d"),
            s("7"),
        ]]
    );

    // (d) UPDATE with WHERE: affected rows + visible effect. (price,
    // not weight: setting two rows to one weight value would trip the
    // UNIQUE KEY u_weight, as it would on MySQL.)
    let (aff, _) = ok_facts(&mut c, "UPDATE items SET price = 1.0 WHERE id <= 2").await;
    assert_eq!(aff, 2, "update affected");
    assert_eq!(
        common::mysql::col(&mut c, "SELECT price FROM items ORDER BY id").await,
        vec![s("1"), s("1"), s("7.75")]
    );

    // (e) Unique-index conflict: MySQL 1062 via the classifier.
    let e = mysql_server_error(
        &mut c,
        "INSERT INTO items (name, weight) VALUES ('dup', 2.5)",
    )
    .await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e:?}");
    assert!(e.message.contains("items.weight"), "{e:?}");

    // (f) DELETE with WHERE, then TRUNCATE (DDL shape: Ok, no count)
    // and DROP.
    let (aff, _) = ok_facts(&mut c, "DELETE FROM items WHERE id = 1").await;
    assert_eq!(aff, 1, "delete affected");
    assert_eq!(
        common::mysql::col(&mut c, "SELECT id FROM items ORDER BY id").await,
        vec![s("2"), s("3")]
    );
    c.query_drop("TRUNCATE items").await.expect("truncate");
    assert_eq!(
        rows(&mut c, "SELECT COUNT(*) FROM items").await,
        vec![vec![s("0")]]
    );
    // ledger: TRUNCATE does not reset the AUTO_INCREMENT watermark on
    // turso (id continues at 4, where MySQL/rocks restart at 1).
    c.query_drop("INSERT INTO items (name) VALUES ('post')")
        .await
        .expect("insert after truncate");
    assert_eq!(
        common::mysql::col(&mut c, "SELECT id FROM items").await,
        vec![s("4")]
    );
    c.query_drop("DROP TABLE items").await.expect("drop table");
    let e = mysql_server_error(&mut c, "SELECT * FROM items").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "{e:?}");

    // (g) Standalone CREATE/DROP INDEX on a fresh table.
    c.query_drop("CREATE TABLE t2 (id INT PRIMARY KEY, v VARCHAR(20), w DOUBLE)")
        .await
        .expect("create t2");
    c.query_drop("CREATE INDEX iv ON t2 (v)")
        .await
        .expect("create index");
    c.query_drop("CREATE UNIQUE INDEX iw ON t2 (w)")
        .await
        .expect("create unique index");
    c.query_drop("INSERT INTO t2 VALUES (1, 'a', 0.5), (2, 'b', 1.5)")
        .await
        .expect("seed t2");
    assert_eq!(
        common::mysql::col(&mut c, "SELECT id FROM t2 WHERE v = 'b'").await,
        vec![s("2")]
    );
    let e = mysql_server_error(&mut c, "INSERT INTO t2 VALUES (3, 'c', 0.5)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{e:?}");
    c.query_drop("DROP INDEX iw ON t2")
        .await
        .expect("drop index");
    c.query_drop("INSERT INTO t2 VALUES (3, 'c', 0.5)")
        .await
        .expect("insert after index drop");

    // (h) 1235 rejection matrix: still-unsupported shapes /
    // engine-specific DDL / unsupported operators and functions.
    // (JOIN / UNION / IN-subquery left this matrix in the allowlist
    // batch -- see turso_setops_e2e for their parity suites.)
    for sql in [
        "SELECT id FROM t2 WHERE id <=> 1",
        "SELECT LEFT('abc', 2)",
        "SELECT id, GROUP_CONCAT(v) FROM t2 GROUP BY id",
        "CREATE TABLE vc (id INT PRIMARY KEY, v VECTOR(3) NOT NULL) ENGINE=columnar",
        "CREATE VECTOR INDEX vi ON t2 (v)",
    ] {
        let e = mysql_server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
        assert!(
            e.message.contains("not supported in this version"),
            "{sql}: {e:?}"
        );
    }

    // VECTOR(N) declared type round-trips for DESCRIBE (T4
    // convention): table_info normalizes to bare `vector`, so the dim
    // is re-read from the CREATE TABLE text and the MySQL face spells
    // `vector(3)` exactly like the rocks engine.
    c.query_drop("CREATE TABLE vec (id INT PRIMARY KEY, v VECTOR(3))")
        .await
        .expect("create vec");
    let desc = rows(&mut c, "DESCRIBE vec").await;
    assert_eq!(desc[1][1], s("vector(3)"), "{desc:?}");

    node.kill_now();
}
