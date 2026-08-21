//! SQL data-plane end-to-end over a real rdb process: the MySQL wire
//! frontend (handshake + native-password auth), the raft-replicated
//! catalog, the MVCC versioned row store and the executor -- DDL, DML,
//! SELECT algebra, EXPLAIN, prepared statements, and auth rejection.

mod common;

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";

async fn connect(node: &common::ProcNode, user: &str, pass: &str) -> mysql_async::Conn {
    let port = node
        .mysql
        .rsplit(':')
        .next()
        .expect("mysql port")
        .parse::<u16>()
        .expect("mysql port digits");
    let opts = || {
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .user(Some(user))
            .pass(Some(pass))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => return c,
            Err(mysql_async::Error::Io(_)) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

/// DDL needs the raft leader; the bootstrap node becomes one within a
/// second or two, so retry the first CREATE until it sticks.
async fn ddl(conn: &mut mysql_async::Conn, sql: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if std::time::Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                panic!("ddl {sql}: {e}")
            }
        }
    }
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect()
}

async fn rows_ordered(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let mut r = rows(conn, sql).await;
    // Queries below carry explicit ORDER BY; sort for stable comparisons.
    r.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    r
}

/// Text-protocol cells are always length-prefixed bytes on the wire,
/// so mysql_async hands back `Value::Bytes` for ints too.
fn int(i: i64) -> MVal {
    MVal::Bytes(i.to_string().into_bytes())
}

fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}

#[tokio::test]
async fn ddl_dml_select_full_flow() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;

    let mut c = connect(&node, "root", PASS).await;

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

    // ---- USE / SET tolerated ----
    c.query_drop("USE rdb").await.expect("use");
    c.query_drop("SET autocommit = 1").await.expect("set");
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
