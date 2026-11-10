//! Allowlist batch 1 parity (JOIN / subquery / set operation / CTE):
//! the SAME statements against the host rocks engine (`USE rdb`) and
//! the embedded turso engine (`USE turso_setops`). Every join kind was
//! smoke-verified against turso 0.9.0-pre.2 before wiring (scratch
//! binary, same crate): INNER/LEFT/RIGHT/FULL/CROSS all execute
//! natively. Engine deltas are asserted engine-side (COMPAT.sql.md
//! ledger (z1)/(z2)): `SELECT *` under USING coalesces the join column
//! on turso where rocks expands both sides; INTERSECT/EXCEPT ALL keep
//! 1235 on turso (grammar takes ALL on UNION only).

use crate::common;

use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;

use common::mysql::{rows, s, spawn_node_mysql_yaml, ER_NOT_SUPPORTED_YET, PASS};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready};

/// Identical schema + seed on both engines (dept 3 has no employee;
/// employees 4/5 no / an unknown dept -- the outer-join probe rows).
const CREATE: &[&str] = &[
    "CREATE TABLE dept (id INT PRIMARY KEY, city VARCHAR(16))",
    "CREATE TABLE emp (id INT PRIMARY KEY, dept_id INT, name VARCHAR(16), pay INT)",
    "INSERT INTO dept VALUES (1, 'sf'), (2, 'ny'), (3, 'la')",
    "INSERT INTO emp VALUES (1, 1, 'ada', 100), (2, 1, 'bob', 200), (3, 2, 'cyd', 300), (4, NULL, 'dee', 50), (5, 9, 'eve', 400)",
];

/// One text cell (None = NULL).
fn cell(v: Option<&str>) -> MVal {
    v.map(s).unwrap_or(MVal::NULL)
}

/// 1-column expected rows.
fn col(vals: &[Option<&str>]) -> Vec<Vec<MVal>> {
    vals.iter().map(|v| vec![cell(*v)]).collect()
}

fn cols2(vals: &[(Option<&str>, Option<&str>)]) -> Vec<Vec<MVal>> {
    vals.iter().map(|(a, b)| vec![cell(*a), cell(*b)]).collect()
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

/// Output column names of one statement (first-branch naming parity).
async fn names(c: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    let rs: Vec<mysql_async::Row> = c.query(sql).await.expect(sql);
    rs.first()
        .map(|r| {
            r.columns_ref()
                .iter()
                .map(|c| c.name_str().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn turso_setops_parity() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-setops-e2e-{}", std::process::id()));
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

    let mut rocks = mysql_root_conn(&node, PASS).await;
    common::mysql::ddl(&mut rocks, "CREATE DATABASE turso_setops").await;
    rocks.query_drop("USE rdb").await.expect("use rdb");
    let mut turso = mysql_root_conn(&node, PASS).await;
    turso
        .query_drop("USE turso_setops")
        .await
        .expect("use turso");
    for sql in CREATE {
        if sql.starts_with("CREATE") {
            common::mysql::ddl(&mut rocks, sql).await;
        } else {
            common::mysql::run(&mut rocks, sql).await;
        }
        turso.query_drop(*sql).await.expect("turso seed");
    }

    // ---- JOIN: INNER (both spellings), qualified cols everywhere ----
    for join in ["INNER JOIN", "JOIN"] {
        same(&mut rocks, &mut turso,
            &format!("SELECT e.name, d.city FROM emp e {join} dept d ON e.dept_id = d.id ORDER BY e.name"),
            cols2(&[(Some("ada"), Some("sf")), (Some("bob"), Some("sf")), (Some("cyd"), Some("ny"))]),
        ).await;
    }
    // LEFT OUTER null-extends the right side
    same(&mut rocks, &mut turso,
        "SELECT e.name, d.city FROM emp e LEFT OUTER JOIN dept d ON e.dept_id = d.id ORDER BY e.name",
        cols2(&[(Some("ada"), Some("sf")), (Some("bob"), Some("sf")), (Some("cyd"), Some("ny")),
                (Some("dee"), None), (Some("eve"), None)]),
    ).await;
    // RIGHT OUTER null-extends the left side (WHERE sees the NULL)
    same(
        &mut rocks,
        &mut turso,
        "SELECT e.name FROM emp e RIGHT JOIN dept d ON e.dept_id = d.id WHERE d.city = 'la'",
        col(&[None]),
    )
    .await;
    same(&mut rocks, &mut turso,
        "SELECT e.name, d.city FROM emp e RIGHT OUTER JOIN dept d ON e.dept_id = d.id ORDER BY d.city, e.name",
        cols2(&[(None, Some("la")), (Some("cyd"), Some("ny")), (Some("ada"), Some("sf")), (Some("bob"), Some("sf"))]),
    ).await;
    // FULL OUTER keeps both unmatched sides
    same(&mut rocks, &mut turso,
        "SELECT e.name, d.city FROM emp e FULL OUTER JOIN dept d ON e.dept_id = d.id ORDER BY e.name",
        cols2(&[(None, Some("la")), (Some("ada"), Some("sf")), (Some("bob"), Some("sf")), (Some("cyd"), Some("ny")),
                (Some("dee"), None), (Some("eve"), None)]),
    ).await;
    // CROSS = full product; compound ON + WHERE filter
    same(
        &mut rocks,
        &mut turso,
        "SELECT COUNT(*) FROM emp CROSS JOIN dept",
        col(&[Some("15")]),
    )
    .await;
    same(&mut rocks, &mut turso,
        "SELECT e.name FROM emp e JOIN dept d ON e.dept_id = d.id AND d.city = 'sf' WHERE e.pay > 100 ORDER BY 1",
        col(&[Some("bob")]),
    ).await;
    // chained join + self-join alias
    same(&mut rocks, &mut turso,
        "SELECT d.city, e.name, x.name FROM dept d JOIN emp e ON e.dept_id = d.id JOIN emp x ON x.dept_id = d.id AND x.id < e.id ORDER BY 1, 2",
        vec![vec![s("sf"), s("bob"), s("ada")]],
    ).await;
    // ---- USING: same-named columns, every join kind ----
    same(
        &mut rocks,
        &mut turso,
        "SELECT name, city FROM emp JOIN dept USING (id) ORDER BY name",
        cols2(&[
            (Some("ada"), Some("sf")),
            (Some("bob"), Some("ny")),
            (Some("cyd"), Some("la")),
        ]),
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name, city FROM emp LEFT JOIN dept USING (id) ORDER BY name",
        cols2(&[
            (Some("ada"), Some("sf")),
            (Some("bob"), Some("ny")),
            (Some("cyd"), Some("la")),
            (Some("dee"), None),
            (Some("eve"), None),
        ]),
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name, city FROM emp RIGHT JOIN dept USING (id) ORDER BY city",
        cols2(&[
            (Some("cyd"), Some("la")),
            (Some("bob"), Some("ny")),
            (Some("ada"), Some("sf")),
        ]),
    )
    .await;
    // ledger: `SELECT *` under USING -- turso coalesces the join column
    // (MySQL shape); rocks expands every side column (id twice).
    assert_eq!(
        rows(
            &mut rocks,
            "SELECT * FROM emp JOIN dept USING (id) ORDER BY pay"
        )
        .await,
        vec![
            vec![s("1"), s("1"), s("ada"), s("100"), s("1"), s("sf")],
            vec![s("2"), s("1"), s("bob"), s("200"), s("2"), s("ny")],
            vec![s("3"), s("2"), s("cyd"), s("300"), s("3"), s("la")]
        ],
        "rocks: every side column (ledger)"
    );
    assert_eq!(
        rows(
            &mut turso,
            "SELECT * FROM emp JOIN dept USING (id) ORDER BY pay"
        )
        .await,
        vec![
            vec![s("1"), s("1"), s("ada"), s("100"), s("sf")],
            vec![s("2"), s("1"), s("bob"), s("200"), s("ny")],
            vec![s("3"), s("2"), s("cyd"), s("300"), s("la")]
        ],
        "turso: join column coalesced (ledger)"
    );

    // ---- subqueries: [NOT] IN / [NOT] EXISTS / scalar / correlated ----
    same(&mut rocks, &mut turso,
        "SELECT name FROM emp WHERE dept_id IN (SELECT id FROM dept WHERE city <> 'la') ORDER BY name",
        col(&[Some("ada"), Some("bob"), Some("cyd")]),
    ).await;
    // NULL dept_id: NOT IN yields unknown, 'dee' drops on both
    same(
        &mut rocks,
        &mut turso,
        "SELECT name FROM emp WHERE dept_id NOT IN (SELECT id FROM dept) ORDER BY name",
        col(&[Some("eve")]),
    )
    .await;
    same(&mut rocks, &mut turso,
        "SELECT name FROM emp e WHERE EXISTS (SELECT 1 FROM dept d WHERE d.id = e.dept_id) ORDER BY name",
        col(&[Some("ada"), Some("bob"), Some("cyd")]),
    ).await;
    same(&mut rocks, &mut turso,
        "SELECT name FROM emp e WHERE NOT EXISTS (SELECT 1 FROM dept d WHERE d.id = e.dept_id) ORDER BY name",
        col(&[Some("dee"), Some("eve")]),
    ).await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name FROM emp WHERE pay > (SELECT AVG(pay) FROM emp) ORDER BY name",
        col(&[Some("cyd"), Some("eve")]),
    )
    .await;
    // correlated scalar in the projection: result parity only (rocks
    // materializes via correlated_map; turso resolves natively)
    same(&mut rocks, &mut turso,
        "SELECT e.name, (SELECT COUNT(*) FROM emp x WHERE x.dept_id = e.dept_id) AS c FROM emp e ORDER BY e.name",
        cols2(&[(Some("ada"), Some("2")), (Some("bob"), Some("2")), (Some("cyd"), Some("1")),
                (Some("dee"), Some("0")), (Some("eve"), Some("1"))]),
    ).await;
    // subquery whose inner FROM is itself a JOIN
    same(&mut rocks, &mut turso,
        "SELECT e.name FROM emp e WHERE e.dept_id IN (SELECT d.id FROM dept d JOIN emp e2 ON e2.dept_id = d.id WHERE d.city = 'sf' AND e2.pay >= 100) ORDER BY 1",
        col(&[Some("ada"), Some("bob")]),
    ).await;

    // ---- set operations: UNION (dedup) vs UNION ALL (multiset) ----
    same(
        &mut rocks,
        &mut turso,
        "SELECT dept_id FROM emp UNION SELECT id FROM dept ORDER BY 1",
        col(&[None, Some("1"), Some("2"), Some("3"), Some("9")]),
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT dept_id FROM emp UNION ALL SELECT id FROM dept ORDER BY 1",
        col(&[
            None,
            Some("1"),
            Some("1"),
            Some("1"),
            Some("2"),
            Some("2"),
            Some("3"),
            Some("9"),
        ]),
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT dept_id FROM emp INTERSECT SELECT id FROM dept ORDER BY 1",
        col(&[Some("1"), Some("2")]),
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT dept_id FROM emp EXCEPT SELECT id FROM dept ORDER BY 1",
        col(&[None, Some("9")]),
    )
    .await;
    // outer ORDER BY / LIMIT (incl. the offset,count swap) on top
    same(
        &mut rocks,
        &mut turso,
        "SELECT name FROM emp UNION SELECT city FROM dept ORDER BY 1 LIMIT 3",
        col(&[Some("ada"), Some("bob"), Some("cyd")]),
    )
    .await;
    same(
        &mut rocks,
        &mut turso,
        "SELECT name FROM emp UNION SELECT city FROM dept ORDER BY 1 DESC LIMIT 2, 3",
        col(&[Some("la"), Some("eve"), Some("dee")]),
    )
    .await;
    let name_sql = "SELECT name AS n FROM emp UNION SELECT city FROM dept LIMIT 1";
    assert_eq!(names(&mut rocks, name_sql).await, vec!["n".to_string()]);
    assert_eq!(names(&mut turso, name_sql).await, vec!["n".to_string()]);
    // parenthesized operands force the parsed tree shape
    same(
        &mut rocks,
        &mut turso,
        "(SELECT id FROM emp WHERE pay < 60) UNION ALL (SELECT id FROM dept) ORDER BY 1",
        col(&[Some("1"), Some("2"), Some("3"), Some("4")]),
    )
    .await;
    // mixed chain: no precedence rule, left fold exactly as parsed
    same(
        &mut rocks,
        &mut turso,
        "SELECT dept_id FROM emp INTERSECT SELECT id FROM dept UNION SELECT id FROM emp ORDER BY 1",
        col(&[Some("1"), Some("2"), Some("3"), Some("4"), Some("5")]),
    )
    .await;
    // compound inside an expression subquery
    same(
        &mut rocks,
        &mut turso,
        "SELECT COUNT(*) FROM emp WHERE id IN (SELECT id FROM emp UNION SELECT id FROM dept)",
        col(&[Some("5")]),
    )
    .await;

    // ---- ledger: INTERSECT/EXCEPT ALL: turso grammar = UNION-only ALL ----
    assert_eq!(
        rows(
            &mut rocks,
            "SELECT dept_id FROM emp INTERSECT ALL SELECT id FROM dept ORDER BY 1"
        )
        .await,
        col(&[Some("1"), Some("2")]),
        "rocks: multiset intersection (ledger)"
    );
    assert_eq!(
        rows(
            &mut rocks,
            "SELECT dept_id FROM emp EXCEPT ALL SELECT id FROM dept ORDER BY 1"
        )
        .await,
        col(&[None, Some("1"), Some("9")]),
        "rocks: multiset subtraction (ledger)"
    );
    for sql in [
        "SELECT dept_id FROM emp INTERSECT ALL SELECT id FROM dept",
        "SELECT dept_id FROM emp EXCEPT ALL SELECT id FROM dept",
    ] {
        let e = mysql_server_error(&mut turso, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
    }
    // derived tables stay outside the turso allowlist
    let e = mysql_server_error(
        &mut turso,
        "SELECT t.n FROM (SELECT name AS n FROM emp) AS t",
    )
    .await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "derived table: {e:?}");

    // ---- CTEs: plain, column aliases, chained, over a join ----
    same(&mut rocks, &mut turso,
        "WITH sf AS (SELECT id FROM dept WHERE city = 'sf') SELECT name FROM emp WHERE dept_id IN (SELECT id FROM sf) ORDER BY name",
        col(&[Some("ada"), Some("bob")]),
    ).await;
    same(
        &mut rocks,
        &mut turso,
        "WITH c (n) AS (SELECT name FROM emp WHERE pay > 200) SELECT n FROM c ORDER BY n",
        col(&[Some("cyd"), Some("eve")]),
    )
    .await;
    same(&mut rocks, &mut turso,
        "WITH a AS (SELECT id, city FROM dept), b AS (SELECT id FROM a WHERE city <> 'la') SELECT name FROM emp WHERE dept_id IN (SELECT id FROM b) ORDER BY name",
        col(&[Some("ada"), Some("bob"), Some("cyd")]),
    ).await;
    same(&mut rocks, &mut turso,
        "WITH rich AS (SELECT e.name AS n FROM emp e JOIN dept d ON e.dept_id = d.id WHERE e.pay >= 300) SELECT n FROM rich UNION SELECT city FROM dept WHERE id = 3 ORDER BY 1",
        col(&[Some("cyd"), Some("la")]),
    ).await;
    // set ops read fine inside an open turso transaction
    turso.query_drop("BEGIN").await.expect("begin");
    same(
        &mut rocks,
        &mut turso,
        "SELECT id FROM dept UNION SELECT 9 ORDER BY 1 DESC LIMIT 1",
        col(&[Some("9")]),
    )
    .await;
    turso.query_drop("COMMIT").await.expect("commit");

    node.kill_now();
}
