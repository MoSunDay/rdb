//! M3 subquery & set-operation e2e over one real rdb process (single
//! node; the cluster gather path is `sql_setop_cluster_e2e.rs`).
//! Extends the M3 pins already in `mysql_compat_e2e.rs` (uncorrelated
//! IN/scalar folding, one correlated IN/EXISTS/scalar each, an
//! INTERSECT/EXCEPT smoke, UNION dedup, literal IN three-valued
//! tables) with:
//! - uncorrelated EXISTS / NOT EXISTS: hit & miss, WHERE inside, empty
//!   inner table, 0/1 cells in the SELECT list;
//! - correlated subqueries: scalar in the SELECT list (inner GROUP BY
//!   aggs, inside COALESCE / CASE), correlated IN, semi-join EXISTS,
//!   anti-join NOT EXISTS (orphans), EXISTS inside EXISTS, scalar
//!   inside scalar, outer-ALIAS references;
//! - NOT IN traps: empty member set, NULL member set, per-row member
//!   sets;
//! - INTERSECT / EXCEPT [DISTINCT|ALL] multiset arithmetic,
//!   self-INTERSECT identity, empty right EXCEPT, mixed chains
//!   (INTERSECT binds tighter than UNION / EXCEPT, one level folds
//!   left -- differentially pinned), arity errors naming the operator,
//!   GROUP BY / aggregate arms;
//! - M1 interplay: GROUP BY DATE(x) HAVING inside EXISTS, IN (SELECT
//!   UPPER(..)), a scalar subquery as a function argument;
//! - narrow loud rejects: WITH RECURSIVE, MINUS / BY NAME, correlated
//!   JOIN conditions, derived-table & skip-level outer refs, multi-row
//!   scalars.
//!
//! Assertions go through brace macros (`col!` / `rows_!` / `err!`) so
//! every case stays ONE line: the suite must fit the 400-line budget
//! for new files, and rustfmt expands long call chains vertically.

mod common;

use common::mysql::{ddl, rows, server_error, world, ER_BAD_FIELD_ERROR, ER_NOT_SUPPORTED_YET};
use common::ProcNode;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

/// The fixture: 4 users (uid 4 has no orders), 4 orders, 3 payments
/// (two on order 100), an EMPTY `ghost` table, duplicate bearers
/// `dup`/`sel` for multiset set ops, a NULL-bearing `nul`, a nullable
/// `tag` for correlated NOT IN, dated `evt` rows for GROUP BY/HAVING,
/// and `city` names for case functions.
async fn setup(tag: &str) -> (ProcNode, mysql_async::Conn) {
    let (node, mut c) = world(tag).await;
    for sql in [
        "CREATE TABLE usr (id BIGINT PRIMARY KEY, name VARCHAR(16) NOT NULL, city VARCHAR(16))",
        "CREATE TABLE ord (id BIGINT PRIMARY KEY, uid BIGINT NOT NULL, amt BIGINT NOT NULL)",
        "CREATE TABLE pay (oid BIGINT, qty BIGINT) PRIMARY KEY(oid, qty)",
        "CREATE TABLE ghost (id BIGINT PRIMARY KEY)",
        "CREATE TABLE dup (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)",
        "CREATE TABLE sel (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)",
        "CREATE TABLE nul (id BIGINT PRIMARY KEY, v BIGINT NULL)",
        "CREATE TABLE tag (id BIGINT PRIMARY KEY, uid BIGINT NULL, lim BIGINT NOT NULL)",
        "CREATE TABLE evt (id BIGINT PRIMARY KEY, uid BIGINT NOT NULL, d DATE, amt BIGINT NOT NULL)",
        "CREATE TABLE city (id BIGINT PRIMARY KEY, nm VARCHAR(16) NOT NULL)",
    ] {
        ddl(&mut c, sql).await;
    }
    for sql in [
        "INSERT INTO usr (id, name, city) VALUES \
         (1,'ada','bj'),(2,'bob',NULL),(3,'cyd','sh'),(4,'dee','bj')",
        "INSERT INTO ord (id, uid, amt) VALUES (100,1,10),(101,1,20),(102,2,30),(103,3,40)",
        "INSERT INTO pay (oid, qty) VALUES (100,5),(100,20),(102,30)",
        "INSERT INTO dup (id, v) VALUES (1,1),(2,2),(3,2),(4,3),(5,3),(6,3)",
        "INSERT INTO sel (id, v) VALUES (1,2),(2,2),(3,3)",
        "INSERT INTO nul (id, v) VALUES (1,1),(2,NULL)",
        "INSERT INTO tag (id, uid, lim) VALUES (1,NULL,1),(2,1,2),(3,2,3)",
        "INSERT INTO evt (id, uid, d, amt) VALUES \
         (1,1,'2024-01-02',5),(2,2,'2024-01-02',7),(3,1,'2024-03-05',9),(4,3,'2024-01-02',11)",
        "INSERT INTO city (id, nm) VALUES (1,'ada'),(2,'Bob')",
    ] {
        c.query_drop(sql)
            .await
            .unwrap_or_else(|e| panic!("seed {sql}: {e}"));
    }
    (node, c)
}

/// One cell as text; NULL spells "" (no fixture cell is empty).
fn txt(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::NULL => String::new(),
        other => panic!("non-text cell {other:?}"),
    }
}

/// The 1235 message of a statement expected to be loudly rejected.
async fn not_supported(conn: &mut mysql_async::Conn, sql: &str) -> String {
    let e = server_error(conn, sql).await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
    e.message
}

/// One output column as sorted text cells.
async fn text_col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = rows(conn, sql).await.iter().map(|r| txt(&r[0])).collect();
    out.sort();
    out
}

/// Whole rows as sorted text cells (row order is unspecified for set
/// operations, and every case here is order-insensitive).
async fn text_rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = rows(conn, sql)
        .await
        .iter()
        .map(|r| r.iter().map(txt).collect())
        .collect();
    out.sort();
    out
}

/// `col! { c, "SELECT ..." => ["1", "2"] }`
macro_rules! col {
    ($c:expr, $sql:expr => [$($v:expr),* $(,)?]) => {
        let want: Vec<String> = vec![$($v.to_string()),*];
        assert_eq!(text_col($c, $sql).await, want, "{}", $sql);
    };
}

/// `rows_! { c, "SELECT ..." => [["1", ""], ["2", "30"]] }` ("" = NULL; the
/// trailing underscore keeps the imported `rows` helper's name free)
macro_rules! rows_ {
    ($c:expr, $sql:expr => [$([$($v:expr),* $(,)?]),* $(,)?]) => {
        let want: Vec<Vec<String>> = vec![$(vec![$($v.to_string()),*]),*];
        assert_eq!(text_rows($c, $sql).await, want, "{}", $sql);
    };
}

/// `err! { c, "SELECT ...", "needle" }`: 1235 with that message.
macro_rules! err {
    ($c:expr, $sql:expr, $needle:expr) => {
        let msg = not_supported($c, $sql).await;
        assert!(
            msg.to_ascii_lowercase()
                .contains(&$needle.to_ascii_lowercase()),
            "{}: {}",
            $needle,
            msg
        );
    };
}

/// `unknown! { c, "SELECT ...", "u.id" }`: 1054 naming the column.
macro_rules! unknown {
    ($c:expr, $sql:expr, $name:expr) => {
        let e = server_error($c, $sql).await;
        assert_eq!(e.code, ER_BAD_FIELD_ERROR, "{}: {}", $name, e.message);
        assert!(e.message.contains($name), "{}", e.message);
    };
}

#[tokio::test]
async fn uncorrelated_exists_counts_and_projects() {
    let (mut node, mut c) = setup("unc-ex").await;
    // Any inner row suffices; NOT EXISTS inverts it; an empty inner
    // table flips both; WHERE inside the subquery decides (amt 10..40).
    col! { &mut c, "SELECT id FROM usr WHERE EXISTS (SELECT 1 FROM ord)" => ["1", "2", "3", "4"] }
    col! { &mut c, "SELECT id FROM usr WHERE NOT EXISTS (SELECT 1 FROM ord)" => [] }
    col! { &mut c, "SELECT COUNT(*) FROM usr WHERE EXISTS (SELECT id FROM ghost)" => ["0"] }
    col! { &mut c, "SELECT id FROM usr WHERE NOT EXISTS (SELECT 1 FROM ghost)" => ["1", "2", "3", "4"] }
    col! { &mut c, "SELECT id FROM usr WHERE EXISTS (SELECT 1 FROM ord WHERE amt > 35)" => ["1", "2", "3", "4"] }
    col! { &mut c, "SELECT id FROM usr WHERE EXISTS (SELECT 1 FROM ord WHERE amt > 45)" => [] }
    // Projection: EXISTS / NOT EXISTS spell 0/1 in the SELECT list.
    col! { &mut c, "SELECT EXISTS (SELECT 1 FROM ghost)" => ["0"] }
    col! { &mut c, "SELECT NOT EXISTS (SELECT 1 FROM ghost)" => ["1"] }
    rows_! { &mut c, "SELECT id, EXISTS (SELECT 1 FROM ord WHERE amt > 35), EXISTS (SELECT 1 FROM ghost) FROM usr" => [["1", "1", "0"], ["2", "1", "0"], ["3", "1", "0"], ["4", "1", "0"]] }
    node.kill_now();
}

#[tokio::test]
async fn correlated_scalar_projection_and_filter() {
    let (mut node, mut c) = setup("cor-scalar").await;
    // Per-outer-row SUM; uid 4 has no orders, empty inner -> NULL. The
    // inner GROUP BY agg answers the same numbers (the group key drops
    // the empty binding, so uid 4 still gets NULL); COALESCE turns
    // that NULL into the default.
    rows_! { &mut c, "SELECT u.id, (SELECT SUM(o.amt) FROM ord o WHERE o.uid = u.id) FROM usr u" => [["1", "30"], ["2", "30"], ["3", "40"], ["4", ""]] }
    rows_! { &mut c, "SELECT u.id, (SELECT SUM(o.amt) FROM ord o WHERE o.uid = u.id GROUP BY o.uid) FROM usr u" => [["1", "30"], ["2", "30"], ["3", "40"], ["4", ""]] }
    rows_! { &mut c, "SELECT id, COALESCE((SELECT MAX(o.amt) FROM ord o WHERE o.uid = usr.id), 0) FROM usr" => [["1", "20"], ["2", "30"], ["3", "40"], ["4", "0"]] }
    // The same scalar drives WHERE, bare and inside arithmetic.
    col! { &mut c, "SELECT id FROM usr WHERE (SELECT COUNT(*) FROM ord o WHERE o.uid = usr.id) >= 2" => ["1"] }
    col! { &mut c, "SELECT u.id FROM usr u WHERE u.id * 10 < (SELECT SUM(o.amt) FROM ord o WHERE o.uid = u.id)" => ["1", "2", "3"] }
    node.kill_now();
}

#[tokio::test]
async fn correlated_exists_semi_and_anti_join() {
    let (mut node, mut c) = setup("semi-anti").await;
    // Semi-join / anti-join shapes; the inner predicate narrows the
    // anti join; correlated IN builds a member set per outer row (the
    // amt threshold moves with u.id, so only uid 1 qualifies); the
    // outer ALIAS binds from inside the subquery.
    col! { &mut c, "SELECT u.id FROM usr u WHERE EXISTS (SELECT 1 FROM ord o WHERE o.uid = u.id)" => ["1", "2", "3"] }
    col! { &mut c, "SELECT u.id FROM usr u WHERE NOT EXISTS (SELECT 1 FROM ord o WHERE o.uid = u.id)" => ["4"] }
    col! { &mut c, "SELECT u.id FROM usr u WHERE NOT EXISTS (SELECT 1 FROM ord o WHERE o.uid = u.id AND o.amt > 25)" => ["1", "4"] }
    col! { &mut c, "SELECT u.id FROM usr u WHERE u.id IN (SELECT o.uid FROM ord o WHERE o.amt > u.id * 15)" => ["1"] }
    col! { &mut c, "SELECT u.id FROM usr u WHERE u.id IN (SELECT o.uid FROM ord o WHERE o.amt = u.id * 10)" => ["1"] }
    node.kill_now();
}

#[tokio::test]
async fn two_level_nesting() {
    let (mut node, mut c) = setup("nest").await;
    // EXISTS inside EXISTS: each level binds against its own outer
    // rows (pay rides on ord, ord on usr).
    col! { &mut c, "SELECT u.id FROM usr u WHERE EXISTS (SELECT 1 FROM ord o WHERE o.uid = u.id AND EXISTS (SELECT 1 FROM pay p WHERE p.oid = o.id AND p.qty >= 20))" => ["1", "2"] }
    // Scalar inside scalar: the inner MIN over a paymentless order is
    // NULL, the comparison is UNKNOWN, that order drops, MAX collapses
    // to the single survivor.
    rows_! { &mut c, "SELECT u.id, (SELECT MAX(o.amt) FROM ord o WHERE o.uid = u.id AND o.amt > (SELECT MIN(p.qty) FROM pay p WHERE p.oid = o.id)) FROM usr u" => [["1", "10"], ["2", ""], ["3", ""], ["4", ""]] }
    node.kill_now();
}

#[tokio::test]
async fn not_in_traps_empty_null_and_correlated() {
    let (mut node, mut c) = setup("notin").await;
    // Empty member set: every row survives. A NULL member is never
    // equal, so NOT IN can no longer be TRUE (three-valued) while IN
    // still answers exact matches. Correlated NOT IN: uid 1 sees an
    // EMPTY member set (survives), uid 2..4 a NULL-bearing one.
    col! { &mut c, "SELECT id FROM usr WHERE id NOT IN (SELECT uid FROM ord WHERE amt > 999)" => ["1", "2", "3", "4"] }
    col! { &mut c, "SELECT id FROM usr WHERE id NOT IN (SELECT v FROM nul)" => [] }
    col! { &mut c, "SELECT id FROM usr WHERE id IN (SELECT v FROM nul)" => ["1"] }
    col! { &mut c, "SELECT u.id FROM usr u WHERE u.id NOT IN (SELECT t.uid FROM tag t WHERE t.lim < u.id)" => ["1"] }
    node.kill_now();
}

#[tokio::test]
async fn intersect_and_except_multiset_semantics() {
    let (mut node, mut c) = setup("setops").await;
    // dup.v = {1,2,2,3,3,3}, sel.v = {2,2,3}: DISTINCT dedups both
    // sides, ALL takes per-distinct-row min / subtracted occurrence
    // counts (INTERSECT: 2 -> min(2,2)=2, 3 -> min(3,1)=1; EXCEPT:
    // 1 -> 1-0, 2 -> 2-2, 3 -> 3-1).
    col! { &mut c, "SELECT v FROM dup INTERSECT SELECT v FROM sel" => ["2", "3"] }
    col! { &mut c, "SELECT v FROM dup INTERSECT ALL SELECT v FROM sel" => ["2", "2", "3"] }
    col! { &mut c, "SELECT v FROM dup EXCEPT SELECT v FROM sel" => ["1"] }
    col! { &mut c, "SELECT v FROM dup EXCEPT ALL SELECT v FROM sel" => ["1", "3", "3"] }
    // Self INTERSECT is the identity; an empty right leaves the left.
    col! { &mut c, "SELECT v FROM dup INTERSECT ALL SELECT v FROM dup" => ["1", "2", "2", "3", "3", "3"] }
    col! { &mut c, "SELECT v FROM dup INTERSECT SELECT v FROM dup" => ["1", "2", "3"] }
    col! { &mut c, "SELECT v FROM dup EXCEPT ALL SELECT v FROM sel WHERE v > 99" => ["1", "2", "2", "3", "3", "3"] }
    col! { &mut c, "SELECT v FROM dup EXCEPT SELECT v FROM sel WHERE v > 99" => ["1", "2", "3"] }
    node.kill_now();
}

#[tokio::test]
async fn mixed_chain_precedence_and_arity_errors() {
    let (mut node, mut c) = setup("chain").await;
    // INTERSECT binds TIGHTER than UNION / EXCEPT, and operators of one
    // level fold LEFT (standard SQL): 4 UNION (3 INTERSECT 3) = {3,4};
    // a pure left fold would answer {3}.
    col! { &mut c, "SELECT 4 UNION SELECT 3 INTERSECT SELECT 3" => ["3", "4"] }
    // So the five-operator chain reads (((1 u 2) u (3 n 3)) u 4) e 4 =
    // {1,2,3,4} minus {4} = {1,2,3}: EXCEPT folds into the running
    // UNION side, never into the INTERSECT'd literal.
    col! { &mut c, "SELECT 1 AS v UNION SELECT 2 UNION SELECT 3 INTERSECT SELECT 3 UNION SELECT 4 EXCEPT SELECT 4" => ["1", "2", "3"] }
    col! { &mut c, "SELECT 1 EXCEPT SELECT 2 UNION SELECT 3" => ["1", "3"] }
    // Parentheses force the plain left-fold reading of the same head.
    col! { &mut c, "(SELECT 1 UNION SELECT 2 UNION SELECT 3) INTERSECT SELECT 3" => ["3"] }
    // Arity mismatch is loud and names the offending operator.
    err! { &mut c, "SELECT 1, 2 INTERSECT SELECT 3", "INTERSECT operands yield different column counts" }
    err! { &mut c, "SELECT 1, 2 EXCEPT ALL SELECT 3", "EXCEPT operands yield different column counts" }
    err! { &mut c, "SELECT 1, 2 UNION SELECT 3", "UNION operands yield different column counts" }
    node.kill_now();
}

#[tokio::test]
async fn setops_over_group_by_and_aggregate_arms() {
    let (mut node, mut c) = setup("set-agg").await;
    // evt dates: '2024-01-02' x3, '2024-03-05' x1. Rows compare as
    // WHOLE tuples, so (d,3) differs from (d,1).
    rows_! { &mut c, "SELECT d, COUNT(*) FROM evt GROUP BY d EXCEPT SELECT d, COUNT(*) FROM evt WHERE uid = 2 GROUP BY d" => [["2024-01-02", "3"], ["2024-03-05", "1"]] }
    rows_! { &mut c, "SELECT d, COUNT(*) FROM evt GROUP BY d INTERSECT SELECT d, COUNT(*) FROM evt WHERE uid <> 2 GROUP BY d" => [["2024-03-05", "1"]] }
    // uid counts are (1:2, 2:1, 3:1); ALL arithmetic against the single
    // (1,1) row the literal arm contributes.
    rows_! { &mut c, "SELECT uid, COUNT(*) FROM evt GROUP BY uid EXCEPT ALL SELECT 1, 2" => [["2", "1"], ["3", "1"]] }
    rows_! { &mut c, "SELECT uid, COUNT(*) FROM evt GROUP BY uid INTERSECT ALL SELECT 1, 2" => [["1", "2"]] }
    col! { &mut c, "SELECT COUNT(*) FROM evt INTERSECT SELECT 4" => ["4"] }
    node.kill_now();
}

#[tokio::test]
async fn subqueries_interplay_with_m1_functions() {
    let (mut node, mut c) = setup("funcs").await;
    // GROUP BY DATE(x) + HAVING inside EXISTS; a scalar subquery as a
    // function argument; functions around IN (SELECT ...); a scalar
    // inside CASE.
    col! { &mut c, "SELECT EXISTS (SELECT 1 FROM evt GROUP BY DATE(d) HAVING COUNT(*) > 2)" => ["1"] }
    col! { &mut c, "SELECT EXISTS (SELECT 1 FROM evt GROUP BY DATE(d) HAVING COUNT(*) > 3)" => ["0"] }
    col! { &mut c, "SELECT CONCAT('n=', (SELECT COUNT(*) FROM ord))" => ["n=4"] }
    col! { &mut c, "SELECT id FROM usr WHERE UPPER(name) IN (SELECT UPPER(nm) FROM city)" => ["1", "2"] }
    rows_! { &mut c, "SELECT id, CASE WHEN (SELECT COUNT(*) FROM ord o WHERE o.uid = usr.id) > 1 THEN 'many' ELSE 'one' END FROM usr" => [["1", "many"], ["2", "one"], ["3", "one"], ["4", "one"]] }
    node.kill_now();
}

#[tokio::test]
async fn narrow_shapes_reject_loudly() {
    let (mut node, mut c) = setup("rejects").await;
    // WITH RECURSIVE stays deferred (P2) and says so; MINUS
    // (non-standard) and the BY NAME quantifier stay parse-rejected;
    // a correlated subquery in a JOIN condition cannot bind (no outer
    // rows exist while the FROM materializes); outer refs inside a
    // DERIVED TABLE of the subquery stay opaque; skip-level refs
    // (grandchild straight to the grandparent scope) cannot chain and
    // surface as an honest 1054 naming the unbindable column; a
    // scalar subquery returning two rows is the MySQL 1242-style error
    // on both the uncorrelated and the correlated path.
    err! { &mut c, "WITH RECURSIVE r (n) AS (SELECT 1) SELECT n FROM r", "recursive" }
    err! { &mut c, "SELECT id FROM usr MINUS SELECT id FROM usr", "Minus" }
    err! { &mut c, "SELECT id FROM usr UNION BY NAME SELECT id FROM usr", "ByName" }
    err! { &mut c, "SELECT u.id FROM usr u JOIN ord o ON o.amt = (SELECT MAX(p.qty) FROM pay p WHERE p.oid = o.id AND u.id = 1)", "JOIN conditions" }
    err! { &mut c, "SELECT u.id FROM usr u WHERE EXISTS (SELECT 1 FROM (SELECT o.amt FROM ord o WHERE o.uid = u.id) d WHERE d.amt > 0)", "outer reference" }
    unknown! { &mut c, "SELECT u.id FROM usr u WHERE EXISTS (SELECT 1 FROM ord o WHERE o.uid = u.id AND o.amt = (SELECT MAX(p.qty) FROM pay p WHERE p.oid = o.id AND u.id = 1))", "u.id" }
    err! { &mut c, "SELECT (SELECT uid FROM ord)", "more than 1 row" }
    err! { &mut c, "SELECT u.id, (SELECT o.amt FROM ord o WHERE o.uid = u.id) FROM usr u", "more than 1 row" }
    node.kill_now();
}

#[tokio::test]
async fn inner_unqualified_column_binds_inner_table() {
    let (mut node, mut c) = setup("inner-k").await;
    // The inner FROM (ord) has its own uid -- 1,1,2,3 -- and the outer
    // tag also carries a uid. The unqualified uid inside the subquery
    // must read the INNER column, so the counts vary per outer row
    // (NULL uid matches nothing, uid 1 two orders, uid 2 one); the
    // lit=lit substitution bug counted all of ord for every non-NULL
    // row (0,4,4). The aliased-inner variant shadows the same way.
    rows_! { &mut c, "SELECT t.lim, (SELECT COUNT(*) FROM ord WHERE uid = t.uid) FROM tag t" => [["1", "0"], ["2", "2"], ["3", "1"]] }
    rows_! { &mut c, "SELECT t.lim, (SELECT COUNT(*) FROM ord o WHERE uid = t.uid) FROM tag t" => [["1", "0"], ["2", "2"], ["3", "1"]] }
    node.kill_now();
}
