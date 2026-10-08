//! SQL data-plane end-to-end over a real rdb process: the MySQL wire
//! frontend (handshake + native-password auth), the raft-replicated
//! catalog, the MVCC versioned row store and the executor -- DDL, DML,
//! SELECT algebra, EXPLAIN, prepared statements, and auth rejection.

mod common;

use common::mysql::{connect_root, ddl, int, rows, rows_ordered, s, ER_PARSE_ERROR, PASS};
use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::OptsBuilder;
use mysql_async::Value as MVal;

#[tokio::test]
async fn ddl_dml_select_full_flow() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;

    let mut c = connect_root(&node).await;

    // ---- DDL ----
    ddl(
        &mut c,
        "CREATE TABLE users (id BIGINT PRIMARY KEY, name VARCHAR(64) NULL, \
         score DOUBLE NOT NULL, active BOOL NOT NULL, avatar BLOB NULL)",
    )
    .await;
    // duplicate without IF NOT EXISTS errors
    assert!(
        c.query_drop("CREATE TABLE users (id BIGINT PRIMARY KEY)")
            .await
            .is_err(),
        "duplicate create must fail"
    );
    ddl(
        &mut c,
        "CREATE TABLE IF NOT EXISTS users (id BIGINT PRIMARY KEY)",
    )
    .await;

    let tables = rows(&mut c, "SHOW TABLES").await;
    assert_eq!(tables, vec![vec![s("users")]]);

    let cols = rows(&mut c, "SHOW COLUMNS FROM users").await;
    assert_eq!(cols.len(), 5);
    assert_eq!(cols[0][0], s("id"));
    assert_eq!(cols[0][3], s("PRI"));

    // ---- INSERT ----
    c.query_drop(
        "INSERT INTO users (id, name, score, active) VALUES (1, 'ada', 9.5, 1), \
                  (2, 'bob', 3.25, 0), (3, NULL, 7.0, 1), (4, 'dee', 3.25, 1)",
    )
    .await
    .expect("insert");
    // missing NOT NULL column errors
    assert!(
        c.query_drop("INSERT INTO users (id, name, active) VALUES (9, 'x', 1)")
            .await
            .is_err(),
        "NOT NULL score must be enforced"
    );

    // ---- SELECT algebra ----
    let got = rows_ordered(
        &mut c,
        "SELECT id, name FROM users WHERE score > 3.0 ORDER BY id DESC",
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![int(1), s("ada")],
            vec![int(2), s("bob")],
            vec![int(3), MVal::NULL],
            vec![int(4), s("dee")],
        ]
    );

    let got = rows(&mut c, "SELECT id FROM users ORDER BY id LIMIT 2 OFFSET 1").await;
    assert_eq!(got, vec![vec![int(2)], vec![int(3)]]);

    let got = rows(&mut c, "SELECT DISTINCT score FROM users ORDER BY score").await;
    assert_eq!(got.len(), 3);

    // aggregates + GROUP BY + HAVING
    let got = rows(
        &mut c,
        "SELECT active, COUNT(*), SUM(score), MIN(name) FROM users \
                            GROUP BY active HAVING COUNT(*) >= 2 ORDER BY active",
    )
    .await;
    // HAVING COUNT(*) >= 2 drops the active=0 group (COUNT=1); the
    // active=1 group is COUNT=3, SUM=9.5+7.0+3.25=19.75, MIN(name)='ada'.
    assert_eq!(
        got,
        vec![vec![
            int(1),
            int(3),
            MVal::Bytes(b"19.75".to_vec()),
            s("ada")
        ]]
    );

    // empty global aggregate
    let got = rows(
        &mut c,
        "SELECT COUNT(*), SUM(score) FROM users WHERE id > 100",
    )
    .await;
    assert_eq!(got, vec![vec![int(0), MVal::NULL]]);

    // three-valued logic: NULL name excluded from equality
    let got = rows(&mut c, "SELECT COUNT(*) FROM users WHERE name = 'ada'").await;
    assert_eq!(got, vec![vec![int(1)]]);
    let got = rows(&mut c, "SELECT COUNT(*) FROM users WHERE name IS NULL").await;
    assert_eq!(got, vec![vec![int(1)]]);

    // ---- UPDATE / DELETE ----
    c.query_drop("UPDATE users SET score = score + 1.0 WHERE id = 2")
        .await
        .expect("update");
    let got = rows(&mut c, "SELECT score FROM users WHERE id = 2").await;
    assert_eq!(got, vec![vec![MVal::Bytes(b"4.25".to_vec())]]);

    c.query_drop("DELETE FROM users WHERE id = 4")
        .await
        .expect("delete");
    let got = rows(&mut c, "SELECT COUNT(*) FROM users").await;
    assert_eq!(got, vec![vec![int(3)]]);

    // ---- prepared statements (`?` binding) ----
    let got: Vec<(i64, Option<String>, f64)> = c
        .exec(
            "SELECT id, name, score FROM users WHERE id > ? ORDER BY id",
            (1i64,),
        )
        .await
        .expect("prepared select");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, 2);
    assert_eq!(got[1].1, None);

    c.exec_drop(
        "INSERT INTO users (id, name, score, active) VALUES (?, ?, ?, ?)",
        (10i64, "eva", 1.5f64, true),
    )
    .await
    .expect("prepared insert");
    let got = rows(&mut c, "SELECT name FROM users WHERE id = 10").await;
    assert_eq!(got, vec![vec![s("eva")]]);

    // mysql-gap open follow-up (2026-10-08 landed): the binary protocol
    // is type-tagged by the announced column, and a `?` placeholder
    // statically types as VAR_STRING -- binding a NUMERIC (or DATE)
    // value used to fail the encoder with an io error and DROP the
    // connection. The cell now coerces compatibly (canonical text,
    // exactly what the text protocol ships) and the connection lives.
    let got: Vec<(String, String)> = c
        .exec(
            "SELECT COALESCE(NULL, ?), COALESCE(NULL, ?)",
            (42i64, 1.5f64),
        )
        .await
        .expect("numeric bind into text-typed placeholder");
    assert_eq!(got, vec![("42".to_string(), "1.5".to_string())]);

    let got: Vec<(String,)> = c
        .exec(
            "SELECT COALESCE(NULL, ?)",
            (mysql_async::Value::Date(2024, 2, 29, 0, 0, 0, 0),),
        )
        .await
        .expect("date bind into text-typed placeholder");
    // (the param may arrive as DATE or midnight DATETIME; either way the
    // cell ships as canonical text instead of dropping the connection)
    assert_eq!(got.len(), 1);
    assert!(got[0].0.starts_with("2024-02-29"), "{}", got[0].0);

    // A projection statically typed DOUBLE surviving an Int runtime
    // cell (CASE types from its first THEN) coerces to the f64 wire form.
    let got: Vec<(f64,)> = c
        .exec("SELECT CASE WHEN ? > 0 THEN 0.5 ELSE 1 END", (-1i64,))
        .await
        .expect("int runtime cell in a double-typed column");
    assert_eq!(got, vec![(1.0,)]);

    // The same connection still works afterwards (no dropped conn).
    let got = rows(&mut c, "SELECT COUNT(*) FROM users").await;
    assert_eq!(got, vec![vec![int(4)]]);

    // LIMIT ? OFFSET ? binds limit-then-offset (the text order):
    // ids > 1 are 2, 3, 10; offset 1 skips 2, limit 2 takes 3 and 10.
    let got: Vec<(i64,)> = c
        .exec(
            "SELECT id FROM users WHERE id > ? ORDER BY id LIMIT ? OFFSET ?",
            (1i64, 2i64, 1i64),
        )
        .await
        .expect("prepared limit offset");
    assert_eq!(got, vec![(3,), (10,)]);

    // the two-placeholder comma form (`LIMIT offset, count`) would
    // bind the values swapped -- it rejects loudly at prepare time
    let err = c
        .exec::<mysql_async::Row, _, _>("SELECT id FROM users LIMIT ?, ?", (1i64, 2i64))
        .await
        .expect_err("LIMIT ?, ? must reject");
    let mysql_async::Error::Server(e) = err else {
        panic!("expected server error for LIMIT ?, ?");
    };
    assert_eq!(e.code, ER_PARSE_ERROR, "errno: {}", e.code);
    assert!(e.message.contains("use LIMIT ? OFFSET ?"), "{}", e.message);

    // ---- EXPLAIN ----
    // (compare raw cells: mysql Value's Debug truncates Bytes to 8 chars)
    let got = rows(
        &mut c,
        "EXPLAIN SELECT id FROM users WHERE score > 1.0 ORDER BY id",
    )
    .await;
    assert!(!got.is_empty(), "explain rows");
    let first = match &got[0][0] {
        MVal::Bytes(b) => String::from_utf8_lossy(b).to_string(),
        other => panic!("first plan line is {other:?}"),
    };
    assert!(first.contains("users"), "plan mentions the table: {first}");

    // ---- USE / cosmetic SET tolerated (session settings rejected) ----
    c.query_drop("USE rdb").await.expect("use");
    // Single-database engine: any other name is MySQL's 1049.
    let err = c.query_drop("USE nodb").await.expect_err("use nodb");
    let mysql_async::Error::Server(e) = err else {
        panic!("expected server error for USE nodb");
    };
    // 1049 = ER_BAD_DB_ERROR ("Unknown database 'x'")
    assert_eq!(e.code, 1049, "errno: {}", e.code);
    assert!(e.message.contains("nodb"), "{}", e.message);
    c.query_drop("SET sql_mode = ''").await.expect("set");
    let got = rows(&mut c, "SHOW TABLES").await;
    assert_eq!(got, vec![vec![s("users")]]);

    // ---- index DDL (M1: catalog-only) ----
    ddl(&mut c, "CREATE INDEX idx_score ON users (score)").await;
    let cols = rows(&mut c, "SHOW COLUMNS FROM users").await;
    assert_eq!(cols.len(), 5, "index DDL does not change columns");
    ddl(&mut c, "DROP INDEX idx_score ON users").await;
    ddl(&mut c, "DROP TABLE IF EXISTS users").await;
    let got = rows(&mut c, "SHOW TABLES").await;
    assert!(got.is_empty());

    // blob round trip
    ddl(
        &mut c,
        "CREATE TABLE blobs (k BIGINT PRIMARY KEY, v BLOB NOT NULL)",
    )
    .await;
    c.exec_drop(
        "INSERT INTO blobs (k, v) VALUES (?, ?)",
        (1i64, vec![0u8, 255, 10, 0]),
    )
    .await
    .expect("blob insert");
    let got = rows(&mut c, "SELECT v FROM blobs WHERE k = 1").await;
    assert_eq!(got, vec![vec![MVal::Bytes(vec![0, 255, 10, 0])]]);

    node.child.kill().ok();
}

#[tokio::test]
async fn native_password_auth_enforced() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-auth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;

    // wrong password rejected at handshake
    let port = node
        .mysql
        .rsplit(':')
        .next()
        .expect("port")
        .parse::<u16>()
        .expect("port digits");
    let try_login = |user: &'static str, pass: &'static str| {
        let opts = OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .user(Some(user))
            .pass(Some(pass));
        mysql_async::Conn::new(opts)
    };
    for (user, pass) in [("root", "wrong-password"), ("nobody", PASS)] {
        let err = try_login(user, pass).await.expect_err("auth must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("1698") || msg.contains("28000"),
            "access denied error for {user}, got: {msg}"
        );
    }
    node.child.kill().ok();
}

/// Concurrent DDL stays isolated: four connections interleave CREATEs
/// of their OWN tables (table ids allocated atomically, so no two
/// tables can ever share one), two connections racing on the SAME name
/// produce exactly one winner (MySQL 1050 for the loser), and no table
/// ever shows another writer's row.
#[tokio::test]
async fn concurrent_create_tables_stay_isolated() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-ddl-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;

    let mut conns: Vec<mysql_async::Conn> = Vec::new();
    for _ in 0..4 {
        conns.push(connect_root(&node).await);
    }

    // 4 conns x 3 tables of their own, all in flight together: 12
    // CREATE + INSERT pairs racing through one catalog.
    futures::future::join_all(conns.iter_mut().enumerate().map(|(i, c)| async move {
        for j in 0..3 {
            ddl(
                c,
                &format!("CREATE TABLE c{i}_t{j} (id BIGINT PRIMARY KEY, tag VARCHAR(16))"),
            )
            .await;
            c.query_drop(format!(
                "INSERT INTO c{i}_t{j} (id, tag) VALUES (1, 'c{i}')"
            ))
            .await
            .expect("own insert");
        }
    }))
    .await;

    // two conns race on the SAME name: exactly one wins
    let mut ra = connect_root(&node).await;
    let mut rb = connect_root(&node).await;
    let create = "CREATE TABLE shared_raced (id BIGINT PRIMARY KEY, tag VARCHAR(16))";
    let raced = futures::future::join_all([ra.query_drop(create), rb.query_drop(create)]).await;
    let mut winners = 0;
    let mut losers: Vec<mysql_async::ServerError> = Vec::new();
    for r in raced {
        match r {
            Ok(()) => winners += 1,
            Err(mysql_async::Error::Server(e)) => losers.push(e),
            Err(other) => panic!("unexpected race error: {other}"),
        }
    }
    assert_eq!(winners, 1, "exactly one CREATE TABLE wins");
    assert_eq!(losers.len(), 1, "the loser reports exactly one error");
    // 1050 = ER_TABLE_EXISTS_ERROR ("table 'x' already exists")
    assert_eq!(losers[0].code, 1050, "loser: {}", losers[0].message);

    // one row lands in the surviving table (whichever conn won)
    ra.query_drop("INSERT INTO shared_raced (id, tag) VALUES (1, 'raced')")
        .await
        .expect("insert into the surviving table");

    // SHOW TABLES lists all 13 (12 own + the raced one)
    let mut names: Vec<String> = rows(&mut conns[0], "SHOW TABLES")
        .await
        .into_iter()
        .map(|r| match &r[0] {
            MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            v => panic!("non-bytes table cell {v:?}"),
        })
        .collect();
    names.sort();
    let mut want: Vec<String> = (0..4)
        .flat_map(|i| (0..3).map(move |j| format!("c{i}_t{j}")))
        .collect();
    want.push("shared_raced".to_string());
    want.sort();
    assert_eq!(names, want, "SHOW TABLES after the concurrent DDL");

    // no crosstalk: every table holds exactly its writer's own row
    for (i, c) in conns.iter_mut().enumerate() {
        for j in 0..3 {
            let got = rows(c, &format!("SELECT id, tag FROM c{i}_t{j}")).await;
            assert_eq!(
                got,
                vec![vec![int(1), s(&format!("c{i}"))]],
                "c{i}_t{j} must hold only its own row"
            );
        }
    }
    let got = rows(&mut ra, "SELECT id, tag FROM shared_raced").await;
    assert_eq!(got, vec![vec![int(1), s("raced")]], "exactly one row");
    node.child.kill().ok();
}
