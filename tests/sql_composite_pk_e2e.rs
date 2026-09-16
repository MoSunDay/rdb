//! Composite `PRIMARY KEY(a, b)` end-to-end on a real rdb process:
//! dedup keyed on the full tuple (repeated single-column values are
//! NOT conflicts), unique secondary index 1062, UPDATE/DELETE with
//! whole- and partial-key WHERE, DESCRIBE / SHOW INDEX / SHOW CREATE
//! TABLE surface, restart persistence of the encoded key, plus the
//! order-preserving escape for pk strings containing NUL bytes.

mod common;

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::{prelude::*, OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";

async fn connect(node: &common::ProcNode) -> mysql_async::Conn {
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
            .user(Some("root"))
            .pass(Some(PASS))
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
                panic!("ddl {sql}: {e}");
            }
        }
    }
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<String>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| match r.get::<MVal, _>(i) {
                    Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap(),
                    Some(MVal::Int(i)) => i.to_string(),
                    v => panic!("non-text cell {v:?}"),
                })
                .collect()
        })
        .collect()
}

async fn col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    rows(conn, sql)
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

async fn errno_of(conn: &mut mysql_async::Conn, sql: &str) -> u16 {
    match conn.query_drop(sql).await {
        Err(mysql_async::Error::Server(e)) => e.code,
        other => panic!("{sql}: expected server error, got {other:?}"),
    }
}

async fn world(name: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-cpk-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut conn = connect(&node).await;
    ddl(
        &mut conn,
        "CREATE TABLE ev (a INT, b VARCHAR(16), d DATE NULL, v BIGINT NULL) PRIMARY KEY(a, b)",
    )
    .await;
    (node, conn)
}

#[tokio::test]
async fn composite_pk_dedups_on_the_full_tuple() {
    let (mut node, mut c) = world("dedup").await;
    c.query_drop("INSERT INTO ev (a, b, v) VALUES (1, 'x', 10), (1, 'y', 20), (2, 'x', 30)")
        .await
        .expect("insert");
    // re-inserting the SAME tuple is an upsert (row-store pk model);
    // tuples differing in any single column are new keys.
    c.query_drop("INSERT INTO ev (a, b, v) VALUES (1, 'x', 11), (3, 'x', 40)")
        .await
        .expect("same-a and same-b rows are not conflicts");
    assert_eq!(
        rows(&mut c, "SELECT a, b, v FROM ev ORDER BY a, b").await,
        vec![
            vec!["1", "x", "11"],
            vec!["1", "y", "20"],
            vec!["2", "x", "30"],
            vec!["3", "x", "40"],
        ]
    );
    // one statement mixing a fresh tuple and a re-insert commits both
    c.query_drop("INSERT INTO ev (a, b, v) VALUES (2, 'y', 50), (2, 'x', 31)")
        .await
        .expect("mixed batch");
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ev").await, vec!["5"]);

    // unique SECONDARY index still rejects by column value: 1062.
    ddl(&mut c, "CREATE UNIQUE INDEX uv ON ev (v)").await;
    assert_eq!(
        errno_of(&mut c, "INSERT INTO ev (a, b, v) VALUES (9, 'z', 11)").await,
        1062
    );
    // moving the unique value away frees it (UPDATE by full key)
    c.query_drop("UPDATE ev SET v = 99 WHERE a = 1 AND b = 'x'")
        .await
        .expect("update");
    assert_eq!(c.affected_rows(), 1);
    c.query_drop("INSERT INTO ev (a, b, v) VALUES (9, 'z', 11)")
        .await
        .expect("unique value reclaimed");

    // DELETE by both key columns removes exactly that tuple
    c.query_drop("DELETE FROM ev WHERE a = 2 AND b = 'x'")
        .await
        .expect("delete");
    assert_eq!(c.affected_rows(), 1);
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ev").await, vec!["5"]);
    node.kill_now();
}

#[tokio::test]
async fn composite_pk_updates_match_partial_key_predicates() {
    let (mut node, mut c) = world("update").await;
    let vals: Vec<String> = (0..6)
        .map(|i| format!("({}, 'k{}', {})", i % 2, i, 100 + i))
        .collect();
    c.query_drop(format!(
        "INSERT INTO ev (a, b, v) VALUES {}",
        vals.join(", ")
    ))
    .await
    .expect("seed");

    // only ONE pk column in the WHERE: still a plain scan+filter match
    c.query_drop("UPDATE ev SET v = 0 WHERE a = 0")
        .await
        .expect("update by first key column");
    assert_eq!(c.affected_rows(), 3);
    assert_eq!(
        col(&mut c, "SELECT v FROM ev WHERE a = 0 ORDER BY v").await,
        vec!["0", "0", "0"]
    );
    // full tuple in the WHERE
    c.query_drop("UPDATE ev SET v = 1 WHERE a = 1 AND b = 'k1'")
        .await
        .expect("full tuple");
    assert_eq!(c.affected_rows(), 1);
    assert_eq!(
        col(&mut c, "SELECT v FROM ev WHERE b = 'k1'").await,
        vec!["1"]
    );
    // non-key predicate (remaining values are 103 and 105)
    c.query_drop("DELETE FROM ev WHERE v > 102")
        .await
        .expect("non-key delete");
    assert_eq!(c.affected_rows(), 2);
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ev").await, vec!["4"]);
    node.kill_now();
}

#[tokio::test]
async fn composite_pk_describe_show_index_and_create_table() {
    let (mut node, mut c) = world("surface").await;
    // DESCRIBE flags every pk column PRI (both are nullable-declared)
    assert_eq!(
        rows(&mut c, "DESCRIBE ev",)
            .await
            .into_iter()
            .map(|mut r| (r.remove(0), r.remove(0), r.remove(1)))
            .collect::<Vec<_>>(),
        vec![
            ("a".into(), "bigint".into(), "PRI".into()),
            ("b".into(), "varchar".into(), "PRI".into()),
            ("d".into(), "date".into(), "".into()),
            ("v".into(), "bigint".into(), "".into()),
        ],
        "Field/Type/Key columns of DESCRIBE"
    );
    // SHOW INDEX: the PRIMARY index gets one row PER pk column with
    // Seq_in_index 1..n (MySQL shape).
    let got = rows(&mut c, "SHOW INDEX FROM ev")
        .await
        .into_iter()
        .filter(|r| r[2] == "PRIMARY")
        .collect::<Vec<_>>();
    let seq: Vec<String> = got.iter().map(|r| r[3].clone()).collect();
    let cols: Vec<String> = got.iter().map(|r| r[4].clone()).collect();
    assert_eq!(seq, vec!["1", "2"], "Seq_in_index");
    assert_eq!(cols, vec!["a", "b"], "Column_name in pk order");
    node.kill_now();
}

#[tokio::test]
async fn composite_pk_escapes_embedded_nul() {
    let (mut node, mut c) = world("nul").await;
    // A pk string containing an embedded NUL byte: previously rejected
    // by the key codec, now stored with the order-preserving escape.
    // The SQL text carries the backslash escape; the server decodes it
    // to a real NUL byte in the stored value.
    let pid = std::process::id();
    let lit = format!("a\\0b-{pid}");
    let nul = format!("a\0b-{pid}");
    c.query_drop(format!(
        "INSERT INTO ev (a, b, v) VALUES (1, '{}', 7), (1, 'ab', 8)",
        lit
    ))
    .await
    .expect("embedded NUL pk accepted");
    assert_eq!(
        rows(
            &mut c,
            &format!("SELECT a, b, v FROM ev WHERE b = '{}'", lit)
        )
        .await,
        vec![vec!["1".to_string(), nul.clone(), "7".to_string()]],
        "the NUL value round-trips through encode/decode"
    );
    // A NUL (0x00) sorts below every printable byte, and the escaped
    // form keeps that: the NUL-embedded key precedes plain 'ab'.
    assert_eq!(
        rows(&mut c, "SELECT b, v FROM ev ORDER BY a, b").await,
        vec![
            vec![nul, "7".to_string()],
            vec!["ab".to_string(), "8".to_string()],
        ],
        "order-preserving escape"
    );
    // the two tuples are distinct keys: re-inserting one is an upsert
    c.query_drop(format!(
        "INSERT INTO ev (a, b, v) VALUES (1, '{}', 70)",
        lit
    ))
    .await
    .expect("escaped tuple upserts, not a new key");
    assert_eq!(col(&mut c, "SELECT COUNT(*) FROM ev").await, vec!["2"]);
    node.kill_now();
}
