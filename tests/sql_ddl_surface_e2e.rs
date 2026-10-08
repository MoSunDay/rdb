//! M4 DDL & session surface e2e over one real rdb process (cluster
//! replication of the same semantics: `sql_ddl_surface_cluster_e2e.rs`).
//! - TRUNCATE: the table-id swap wipes rows AND secondary/unique index
//!   entries (old unique values re-insertable), resets AUTO_INCREMENT,
//!   keeps INSERT..SELECT working, rejects inside an open txn;
//! - RENAME (+ ALTER .. RENAME TO): catalog-only -- data, indexes and
//!   sequence reservations ride along; the old name 1146s (prepared
//!   re-exec too, never a hang); busy-target / missing-source /
//!   cross-db / multi-pair reject, case-only rename is allowed;
//! - ALTER ADD/DROP INDEX + CREATE/DROP INDEX + inline KEY / UNIQUE
//!   KEY in CREATE TABLE: index lookups, loud 1062 unique pre-checks,
//!   scan fallback after DROP INDEX, composite/prefix keys -> 1235;
//! - SHOW CREATE TABLE (deterministic render + round-trip), DATABASES,
//!   [GLOBAL|SESSION] VARIABLES [LIKE] (case-blind, % / _ wildcards,
//!   WHERE rejected), STATUS (Uptime);
//! - session funcs bound per execution: USER family, CONNECTION_ID
//!   (stable per conn, distinct across conns), DATABASE()/SCHEMA()
//!   answering across a prepared re-execute; USE of any database but
//!   the canonical one 1049s.

mod common;

use std::time::Duration;

use common::mysql::{
    connect_root, ddl, one, rows, run, server_error, world, ER_DUP_ENTRY, ER_NOT_SUPPORTED_YET,
    ER_NO_SUCH_TABLE,
};
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

/// One cell as text (Bytes and Int both spell out over the wire).
fn txt(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => panic!("non-text cell {other:?}"),
    }
}

/// (name, value) pairs of a SHOW VARIABLES / SHOW STATUS rowset.
async fn kv(conn: &mut mysql_async::Conn, sql: &str) -> Vec<(String, String)> {
    rows(conn, sql)
        .await
        .into_iter()
        .map(|r| (txt(&r[0]), txt(&r[1])))
        .collect()
}

/// First-column cells of a rowset, comma-joined (expected shapes).
async fn text(conn: &mut mysql_async::Conn, sql: &str) -> String {
    rows(conn, sql)
        .await
        .iter()
        .map(|r| txt(&r[0]))
        .collect::<Vec<_>>()
        .join(",")
}

/// Whole rowset as `a|b` lines joined by newlines ("" = no rows).
async fn grid(conn: &mut mysql_async::Conn, sql: &str) -> String {
    rows(conn, sql)
        .await
        .iter()
        .map(|r| r.iter().map(txt).collect::<Vec<_>>().join("|"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rendered DDL cell of `SHOW CREATE TABLE t`.
async fn create_table_sql(conn: &mut mysql_async::Conn, table: &str) -> String {
    txt(&rows(conn, &format!("SHOW CREATE TABLE {table}")).await[0][1])
}

/// First plan line of EXPLAIN (index-vs-scan verdicts).
async fn plan(conn: &mut mysql_async::Conn, sql: &str) -> String {
    txt(&one(conn, &format!("EXPLAIN {sql}")).await)
}

#[tokio::test]
async fn truncate_wipes_rows_indexes_and_resets_autoinc() {
    let (mut node, mut c) = world("trunc").await;
    ddl(&mut c, "CREATE TABLE tr (id BIGINT PRIMARY KEY AUTO_INCREMENT, u VARCHAR(32) NOT NULL, v VARCHAR(32) NULL)").await;
    ddl(&mut c, "CREATE UNIQUE INDEX uk_u ON tr (u)").await;
    ddl(&mut c, "CREATE INDEX idx_v ON tr (v)").await;
    let sql = "INSERT INTO tr (u, v) VALUES ('t1', 'red'), ('t2', 'red'), ('t3', 'blue')";
    run(&mut c, sql).await;
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM tr").await, "3");

    ddl(&mut c, "TRUNCATE TABLE tr").await;
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM tr").await, "0");

    // Unique entries swept with the old table id: re-using 't1' is not
    // vetoed, and the fresh table's AUTO_INCREMENT starts at 1.
    run(&mut c, "INSERT INTO tr (u, v) VALUES ('t1', 'green')").await;
    let want = "1";
    assert_eq!(grid(&mut c, "SELECT id FROM tr WHERE u='t1'").await, want);
    // Secondary entries swept too: an index lookup finds no old 'red'.
    assert_eq!(grid(&mut c, "SELECT id FROM tr WHERE v='red'").await, "");
    // The unique index stays LIVE for fresh rows: a second 't1' vetoes.
    let e = server_error(&mut c, "INSERT INTO tr (u, v) VALUES ('t1', 'x')").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{}", e.message);

    // INSERT .. SELECT refills through the fresh id
    let sql =
        "CREATE TABLE src (id BIGINT PRIMARY KEY, u VARCHAR(32) NOT NULL, v VARCHAR(32) NULL)";
    ddl(&mut c, sql).await;
    let sql = "INSERT INTO src (id, u, v) VALUES (1, 's1', 'x'), (2, 's2', 'y')";
    run(&mut c, sql).await;
    run(&mut c, "INSERT INTO tr (u, v) SELECT u, v FROM src").await;
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM tr").await, "3");

    // DDL semantics: rejected inside an open txn, rows survive it
    run(&mut c, "BEGIN").await;
    let e = server_error(&mut c, "TRUNCATE TABLE tr").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    run(&mut c, "ROLLBACK").await;
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM tr").await, "3");
    node.kill_now();
}

#[tokio::test]
async fn rename_moves_data_indexes_and_sequence_verbatim() {
    let (mut node, mut c) = world("ren").await;
    let sql = "CREATE TABLE a (id BIGINT PRIMARY KEY AUTO_INCREMENT, v VARCHAR(64) NULL)";
    ddl(&mut c, sql).await;
    ddl(&mut c, "CREATE INDEX idx_v ON a (v)").await;
    let sql = "INSERT INTO a (v) VALUES ('one'), ('two'), ('three')";
    run(&mut c, sql).await;
    // Prepared against the LIVE name: its re-exec after the rename
    // must error cleanly (never hang).
    let stmt = c.prep("SELECT COUNT(*) FROM a").await.expect("prep");

    ddl(&mut c, "RENAME TABLE a TO b").await;
    // Data visible under the new name immediately
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM b").await, "3");
    let want = "1|one\n2|two\n3|three";
    assert_eq!(grid(&mut c, "SELECT id, v FROM b ORDER BY id").await, want);
    // The old name is gone: 1146 on reads AND writes
    for sql in ["SELECT * FROM a", "INSERT INTO a (v) VALUES ('x')"] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NO_SUCH_TABLE, "{sql}: {}", e.message);
    }
    // The timeout wrap is the assertion: a hang fails the test.
    let reexec: Result<Vec<mysql_async::Row>, mysql_async::Error> =
        tokio::time::timeout(Duration::from_secs(10), c.exec(stmt, ()))
            .await
            .expect("prepared re-exec on the old name must answer, not hang");
    match reexec {
        Err(mysql_async::Error::Server(e)) => {
            assert_eq!(e.code, ER_NO_SUCH_TABLE, "{}", e.message);
        }
        other => panic!("prepared re-exec on the old name must 1146, got {other:?}"),
    }
    // The secondary index still answers under the new name
    let want = "2";
    assert_eq!(grid(&mut c, "SELECT id FROM b WHERE v='two'").await, want);
    // AUTO_INCREMENT continues across the rename (no reset): the next
    // id sits above every handed-out id -- batch reservations move
    // verbatim, so it may sit above a tight successor (gap accepted).
    run(&mut c, "INSERT INTO b (v) VALUES ('four')").await;
    let id4: i64 = grid(&mut c, "SELECT id FROM b WHERE v='four'")
        .await
        .parse()
        .expect("id digits");
    assert!(id4 > 3, "counter carried over, next id {id4}");

    // ALTER TABLE .. RENAME TO is the same executor; data + counter ride
    ddl(&mut c, "ALTER TABLE b RENAME TO c").await;
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM c").await, "4");
    run(&mut c, "INSERT INTO c (v) VALUES ('five')").await;
    let id5: i64 = grid(&mut c, "SELECT id FROM c WHERE v='five'")
        .await
        .parse()
        .expect("id digits");
    assert!(
        id5 > id4,
        "counter survives the second rename: {id5} vs {id4}"
    );

    // Rejections: busy target, missing source, cross-db, multi-pair.
    ddl(&mut c, "CREATE TABLE occupied (id BIGINT PRIMARY KEY)").await;
    for (sql, code, why) in [
        ("RENAME TABLE c TO occupied", 1050u16, "target exists"),
        (
            "RENAME TABLE missing_src TO elsewhere",
            ER_NO_SUCH_TABLE,
            "missing source",
        ),
        (
            "RENAME TABLE c TO other_db.target",
            ER_NOT_SUPPORTED_YET,
            "cross-db",
        ),
        (
            "RENAME TABLE c TO d, occupied TO e",
            ER_NOT_SUPPORTED_YET,
            "multi-pair",
        ),
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, code, "{why} ({sql}): {}", e.message);
    }
    // Case-only rename is allowed (identifier lookups are case-blind)
    ddl(&mut c, "RENAME TABLE c TO C").await;
    assert_eq!(grid(&mut c, "SELECT COUNT(*) FROM C").await, "5");
    node.kill_now();
}

#[tokio::test]
async fn index_ddl_forms_and_unique_prechecks() {
    let (mut node, mut c) = world("idx").await;
    // One column per index: on-disk entries key by (table_id, col_pos),
    // so two indexes sharing a column would sweep each other's entries.
    let sql =
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, u BIGINT NULL, w BIGINT NULL)";
    ddl(&mut c, sql).await;
    run(&mut c, "INSERT INTO t (id, v, u, w) VALUES (1, 'red', 10, 100), (2, 'blue', 20, 200), (3, 'red', 30, 300)").await;

    // CREATE INDEX: equality lookups route through the new index
    ddl(&mut c, "CREATE INDEX idx_v ON t (v)").await;
    let want = "IndexScan idx_v -> 2 pks";
    let q = "SELECT id FROM t WHERE v='red'";
    assert_eq!(plan(&mut c, q).await, want);
    let want = "1,3";
    let q = "SELECT id FROM t WHERE v='red' ORDER BY id";
    assert_eq!(text(&mut c, q).await, want);
    // ALTER TABLE ADD INDEX runs the same executor
    ddl(&mut c, "ALTER TABLE t ADD INDEX idx_u (u)").await;
    let want = "IndexScan idx_u -> 1 pks";
    assert_eq!(plan(&mut c, "SELECT id FROM t WHERE u=20").await, want);
    assert_eq!(grid(&mut c, "SELECT id FROM t WHERE u=20").await, "2");

    // ADD UNIQUE pre-checks existing rows: 'red' is duplicated -> loud
    let e = server_error(&mut c, "CREATE UNIQUE INDEX uk_v ON t (v)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{}", e.message);
    // On a clean column the unique index vetoes from now on
    ddl(&mut c, "ALTER TABLE t ADD UNIQUE INDEX uk_w (w)").await;
    let q = "INSERT INTO t (id, v, u, w) VALUES (4, 'green', 40, 200)";
    let e = server_error(&mut c, q).await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{}", e.message);

    // DROP INDEX (statement form) falls back to a scan, still correct
    ddl(&mut c, "DROP INDEX idx_v ON t").await;
    let want = "SeqScan t";
    let q = "SELECT id FROM t WHERE v='red'";
    assert_eq!(plan(&mut c, q).await, want);
    let want = "1,3";
    let q = "SELECT id FROM t WHERE v='red' ORDER BY id";
    assert_eq!(text(&mut c, q).await, want);
    // ALTER TABLE DROP INDEX form: the veto goes away with the entry
    ddl(&mut c, "ALTER TABLE t DROP INDEX uk_w").await;
    let sql = "INSERT INTO t (id, v, u, w) VALUES (5, 'gray', 50, 200)";
    run(&mut c, sql).await;
    let want = "2";
    let q = "SELECT COUNT(*) FROM t WHERE w=200";
    assert_eq!(grid(&mut c, q).await, want);

    // Inline KEY / UNIQUE KEY inside CREATE TABLE
    ddl(&mut c, "CREATE TABLE k (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, u BIGINT NULL, KEY k_v (v), UNIQUE KEY uk_ku (u))").await;
    let sql = "INSERT INTO k (id, v, u) VALUES (1, 'x', 10), (2, 'y', 20)";
    run(&mut c, sql).await;
    let want = "IndexScan k_v -> 1 pks";
    let q = "SELECT id FROM k WHERE v='x'";
    assert_eq!(plan(&mut c, q).await, want);
    let e = server_error(&mut c, "INSERT INTO k (id, v, u) VALUES (3, 'z', 10)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "{}", e.message);

    // Composite / prefix keys: loud 1235 (P2 deferral)
    for sql in [
        "CREATE INDEX comp ON t (v, u)",
        "CREATE INDEX pref ON t (v(10))",
        "ALTER TABLE t ADD INDEX comp2 (v, u)",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
    }
    node.kill_now();
}

#[tokio::test]
async fn show_create_table_databases_variables_and_status() {
    let (mut node, mut c) = world("show").await;
    let sql = "CREATE TABLE n (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, KEY k_v (v))";
    ddl(&mut c, sql).await;
    ddl(&mut c, "CREATE TABLE uq (id BIGINT NOT NULL PRIMARY KEY, tag VARCHAR(16) NOT NULL, UNIQUE KEY uk_tag (tag))").await;
    let sql = "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL)";
    ddl(&mut c, sql).await;

    // Deterministic MySQL-style render: exact shapes
    let want_n = "CREATE TABLE `n` (\n  `id` bigint NOT NULL,\n  `v` varchar NULL DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `k_v` (`v`)\n) ENGINE=InnoDB";
    assert_eq!(create_table_sql(&mut c, "n").await, want_n);
    let want_uq = "CREATE TABLE `uq` (\n  `id` bigint NOT NULL,\n  `tag` varchar NOT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `uk_tag` (`tag`)\n) ENGINE=InnoDB";
    assert_eq!(create_table_sql(&mut c, "uq").await, want_uq);
    let want_ai = "CREATE TABLE `ai` (\n  `id` bigint NOT NULL AUTO_INCREMENT,\n  `v` varchar NULL DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB";
    assert_eq!(create_table_sql(&mut c, "ai").await, want_ai);
    // Round-trip: the rendered DDL re-creates the identical shape
    ddl(&mut c, "DROP TABLE n").await;
    ddl(&mut c, want_n).await;
    assert_eq!(create_table_sql(&mut c, "n").await, want_n);

    // SHOW VARIABLES: the full sysvar table, no filter
    let vars = kv(&mut c, "SHOW VARIABLES").await;
    assert!(vars.len() >= 25, "a full sysvar table, got {}", vars.len());
    for (name, val) in [
        ("max_connections", "151"),
        ("auto_increment_increment", "1"),
        ("version", "8.0.32-rdb"),
    ] {
        assert!(
            vars.iter().any(|(n, v)| n == name && v == val),
            "{name}={val} missing"
        );
    }
    // LIKE is case-insensitive with MySQL's % / _ wildcards
    let chars = "character_set_client,character_set_connection,character_set_database,character_set_results";
    assert_eq!(text(&mut c, "SHOW VARIABLES LIKE 'char%'").await, chars);
    assert_eq!(text(&mut c, "SHOW VARIABLES LIKE 'CHAR%'").await, chars); // case-blind
    let timeouts = "interactive_timeout,net_read_timeout,net_write_timeout,wait_timeout";
    let q = "SHOW VARIABLES LIKE '%timeout'";
    assert_eq!(text(&mut c, q).await, timeouts);
    // `_` matches exactly one character: _ax_connections -> max_..
    let want = "max_connections";
    let q = "SHOW VARIABLES LIKE '_ax_connections'";
    assert_eq!(text(&mut c, q).await, want);
    // GLOBAL / SESSION prefixes share the one sysvar table
    let maxes = "max_allowed_packet,max_connections";
    let q = "SHOW GLOBAL VARIABLES LIKE 'max%'";
    assert_eq!(text(&mut c, q).await, maxes);
    let q = "SHOW SESSION VARIABLES LIKE 'max%'";
    assert_eq!(text(&mut c, q).await, maxes);
    // The WHERE form is rejected (LIKE only)
    let q = "SHOW VARIABLES WHERE Variable_name = 'max_connections'";
    let e = server_error(&mut c, q).await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);

    // SHOW STATUS: only counters answerable honestly; Uptime >= 0
    let status = kv(&mut c, "SHOW STATUS").await;
    let uptime = status
        .iter()
        .find(|(n, _)| n == "Uptime")
        .expect("Uptime row");
    assert!(uptime.1.parse::<i64>().unwrap() >= 0);
    assert_eq!(text(&mut c, "SHOW STATUS LIKE 'Up%'").await, "Uptime");

    // SHOW DATABASES: the one hosted database; any other USE target is
    // MySQL's 1049 (the engine registers no other database)
    assert_eq!(text(&mut c, "SHOW DATABASES").await, "rdb");
    let e = server_error(&mut c, "USE probe_db").await;
    // 1049 = ER_BAD_DB_ERROR
    assert_eq!(e.code, 1049, "{}", e.message);
    run(&mut c, "USE rdb").await;
    node.kill_now();
}

#[tokio::test]
async fn session_functions_bind_per_execution() {
    let (mut node, mut c) = world("sess").await;
    // USER() renders the authenticated login as user@peer-host; the
    // CURRENT_USER / SESSION_USER spellings answer identically.
    let user = grid(&mut c, "SELECT USER()").await;
    assert!(user.starts_with("root@"), "login prefix, got {user}");
    for f in ["CURRENT_USER()", "SESSION_USER()"] {
        assert_eq!(grid(&mut c, &format!("SELECT {f}")).await, user, "{f}");
    }
    // CONNECTION_ID(): stable within the conn, distinct across conns
    let id = one(&mut c, "SELECT CONNECTION_ID()").await;
    assert_eq!(one(&mut c, "SELECT CONNECTION_ID()").await, id);
    let mut c2 = connect_root(&node).await;
    assert_ne!(one(&mut c2, "SELECT CONNECTION_ID()").await, id);

    // DATABASE()/SCHEMA(): the default db; only the canonical name USEs
    assert_eq!(grid(&mut c, "SELECT DATABASE()").await, "rdb");
    assert_eq!(grid(&mut c, "SELECT SCHEMA()").await, "rdb");
    // Per-EXECUTION binding: prepared BEFORE the USE attempt, executed
    // after it -- the rejected USE leaves the default db in place and
    // the exec still answers (this shape once hung).
    let stmt = c.prep("SELECT DATABASE()").await.expect("prep db");
    let e = server_error(&mut c, "USE moved_db").await;
    assert_eq!(e.code, 1049, "{}", e.message);
    let got: Vec<(String,)> = tokio::time::timeout(Duration::from_secs(10), c.exec(stmt, ()))
        .await
        .expect("exec after USE must answer, not hang")
        .expect("exec database");
    assert_eq!(got, vec![("rdb".to_string(),)]);
    node.kill_now();
}
