//! Phase-1 MySQL language-surface e2e over one real rdb process:
//! UNION [ALL], non-recursive CTEs (WITH), derived tables, and
//! uncorrelated subqueries (scalar + IN), plus the loud rejections
//! (INTERSECT/EXCEPT, WITH RECURSIVE, correlated refs, arity
//! mismatch, multi-row scalars), the three-valued logic of
//! IN / NOT IN / NOT, and compound EXPLAIN.

mod common;

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

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

/// ORDER BY is part of what we exercise here; comparisons sort rows
/// textually so both orders of arrival pass.
async fn rows_stable(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let mut r = rows(conn, sql).await;
    r.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    r
}

fn int(i: i64) -> MVal {
    MVal::Bytes(i.to_string().into_bytes())
}

fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}

fn f(v: f64) -> MVal {
    MVal::Bytes(format!("{v}").into_bytes())
}

async fn err_contains(conn: &mut mysql_async::Conn, sql: &str, needle: &str) {
    let e = conn
        .query::<mysql_async::Row, _>(sql)
        .await
        .err()
        .unwrap_or_else(|| panic!("expected error containing {needle:?} for: {sql}"));
    assert!(
        e.to_string().to_ascii_lowercase().contains(needle),
        "error for {sql} was {e}"
    );
}

async fn setup(conn: &mut mysql_async::Conn) {
    ddl(
        conn,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(16) NULL, score DOUBLE NOT NULL)",
    )
    .await;
    ddl(
        conn,
        "CREATE TABLE u (id BIGINT PRIMARY KEY, ref_id BIGINT NOT NULL)",
    )
    .await;
    conn.query_drop(
        "INSERT INTO t (id, name, score) VALUES \
         (1,'a',1.5),(2,'b',2.5),(3,NULL,3.5),(4,'d',4.5)",
    )
    .await
    .expect("seed t");
    conn.query_drop("INSERT INTO u (id, ref_id) VALUES (10,1),(11,2),(12,9)")
        .await
        .expect("seed u");
}

#[tokio::test]
async fn union_all_concatenates_and_union_dedups() {
    let dir = std::env::temp_dir().join(format!("rdb-compat-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;
    setup(&mut c).await;

    // UNION ALL keeps both sides' duplicates, tail ORDER BY applies
    // to the compound.
    let got = rows_stable(
        &mut c,
        "SELECT id FROM t UNION ALL SELECT ref_id FROM u ORDER BY 1",
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![int(1)],
            vec![int(1)],
            vec![int(2)],
            vec![int(2)],
            vec![int(3)],
            vec![int(4)],
            vec![int(9)]
        ],
        "UNION ALL concat"
    );

    // plain UNION dedups (NULLs equal, exactly one survives).
    let got = rows_stable(&mut c, "SELECT name FROM t UNION SELECT 'a' FROM u").await;
    assert_eq!(
        got,
        vec![vec![s("a")], vec![s("b")], vec![s("d")], vec![MVal::NULL]],
        "UNION dedup incl. NULL"
    );

    // int column UNION double column widens to double.
    let got = rows_stable(
        &mut c,
        "SELECT id FROM t WHERE id = 1 UNION ALL SELECT score FROM t WHERE id = 1",
    )
    .await;
    assert_eq!(got, vec![vec![f(1.0)], vec![f(1.5)]], "numeric widen");

    // parenthesized operand with its own LIMIT, then outer tail.
    let got = rows_stable(
        &mut c,
        "(SELECT id FROM t ORDER BY id DESC LIMIT 1) UNION ALL (SELECT id FROM t ORDER BY id LIMIT 1)",
    )
    .await;
    assert_eq!(got, vec![vec![int(1)], vec![int(4)]], "nested operands");

    // arity mismatch is a loud error.
    err_contains(
        &mut c,
        "SELECT id, name FROM t UNION ALL SELECT id FROM u",
        "column counts",
    )
    .await;

    // INTERSECT / EXCEPT reject loudly (Phase-1 scope: UNION only).
    err_contains(&mut c, "SELECT id FROM t INTERSECT SELECT id FROM u", "set").await;
    err_contains(&mut c, "SELECT id FROM t EXCEPT SELECT id FROM u", "set").await;

    // tail LIMIT/OFFSET on the compound.
    let got = rows(
        &mut c,
        "SELECT id FROM t UNION ALL SELECT ref_id FROM u ORDER BY 1 LIMIT 2 OFFSET 3",
    )
    .await;
    assert_eq!(got, vec![vec![int(2)], vec![int(3)]], "limit+offset");

    // EXPLAIN renders a compound plan.
    let plan = rows(
        &mut c,
        "EXPLAIN SELECT id FROM t WHERE id > 1 UNION SELECT ref_id FROM u",
    )
    .await;
    let text = format!("{plan:?}");
    assert!(text.contains("Union"), "compound explain: {text}");
    node.kill_now();
}

#[tokio::test]
async fn ctes_derived_and_subqueries() {
    let dir = std::env::temp_dir().join(format!("rdb-compat-e2e2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;
    setup(&mut c).await;

    // CTE referenced twice + join against a base table.
    let got = rows_stable(
        &mut c,
        "WITH scored AS (SELECT id, score FROM t WHERE score > 2.0) \
         SELECT a.id, b.id FROM scored a JOIN scored b ON a.id + 1 = b.id",
    )
    .await;
    assert_eq!(
        got,
        vec![vec![int(2), int(3)], vec![int(3), int(4)]],
        "cte self-join"
    );

    // column alias list renames; later CTE sees earlier one.
    let got = rows_stable(
        &mut c,
        "WITH x (n) AS (SELECT id FROM t WHERE id <= 2), \
         y (m) AS (SELECT n + 10 FROM x) SELECT m FROM y ORDER BY m",
    )
    .await;
    assert_eq!(
        got,
        vec![vec![int(11)], vec![int(12)]],
        "cte alias list + chain"
    );

    // derived table with WHERE + aggregation inside.
    let got = rows(
        &mut c,
        "SELECT d.avg_score FROM (SELECT AVG(score) AS avg_score FROM t WHERE id < 4) d",
    )
    .await;
    assert_eq!(got, vec![vec![f(2.5)]], "derived table");

    // IN (SELECT ...): membership + NOT IN.
    let got = rows_stable(
        &mut c,
        "SELECT id FROM t WHERE id IN (SELECT ref_id FROM u)",
    )
    .await;
    assert_eq!(got, vec![vec![int(1)], vec![int(2)]], "IN subquery");
    let got = rows_stable(
        &mut c,
        "SELECT id FROM t WHERE id NOT IN (SELECT ref_id FROM u) ORDER BY 1",
    )
    .await;
    assert_eq!(got, vec![vec![int(3)], vec![int(4)]], "NOT IN subquery");

    // scalar subquery: value, empty -> NULL, multi-row -> error.
    let got = rows(&mut c, "SELECT (SELECT MAX(score) FROM t)").await;
    assert_eq!(got, vec![vec![f(4.5)]], "scalar subquery");
    let got = rows(&mut c, "SELECT (SELECT score FROM t WHERE id = 99)").await;
    assert_eq!(got, vec![vec![MVal::NULL]], "scalar subquery empty -> NULL");
    err_contains(&mut c, "SELECT (SELECT score FROM t)", "more than 1 row").await;

    // correlated subqueries reject loudly (Phase-1 scope).
    err_contains(
        &mut c,
        "SELECT id FROM t WHERE id IN (SELECT ref_id FROM u WHERE u.ref_id = t.id)",
        "correlated",
    )
    .await;

    // WITH RECURSIVE rejects loudly.
    err_contains(
        &mut c,
        "WITH RECURSIVE r (n) AS (SELECT 1) SELECT n FROM r",
        "recursive",
    )
    .await;

    // subquery inside a CTE body also works.
    let got = rows_stable(
        &mut c,
        "WITH hit AS (SELECT id FROM t WHERE id IN (SELECT ref_id FROM u)) SELECT COUNT(*) FROM hit",
    )
    .await;
    assert_eq!(got, vec![vec![int(2)]], "subquery inside CTE");
    node.kill_now();
}

/// Three-valued logic: NULL is UNKNOWN, not FALSE. `x IN (NULL, ...)`
/// stays NULL unless an exact match, `NOT IN` over a NULL element is
/// NULL for every non-member, and `NOT` flips known booleans only
/// (MySQL's truth tables, asserted through SELECT cells).
#[tokio::test]
async fn not_and_in_are_three_valued() {
    let dir = std::env::temp_dir().join(format!("rdb-compat-3vl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;

    for (sql, want) in [
        ("SELECT 1 IN (NULL, 1)", int(1)),
        ("SELECT 2 IN (NULL, 1)", MVal::NULL),
        ("SELECT 2 IN (NULL, 3)", MVal::NULL),
        ("SELECT 1 NOT IN (NULL, 2)", MVal::NULL),
        ("SELECT 3 NOT IN (NULL, 2)", MVal::NULL),
        ("SELECT NOT NULL", MVal::NULL),
        ("SELECT NOT 0", int(1)),
        ("SELECT NOT 5", int(0)),
    ] {
        assert_eq!(rows(&mut c, sql).await, vec![vec![want]], "{sql}");
    }
    node.kill_now();
}
