//! LEFT/RIGHT [OUTER] JOIN e2e over one real rdb process (single node:
//! the cluster-mode join gather is covered by `sql_join_cluster_e2e.rs`,
//! which exercises the same `join_sources` loop cluster-wide). The
//! executor null-extends the unmatched side of an OUTER join; covered:
//! - LEFT JOIN pads the right side with NULLs, RIGHT JOIN pads the
//!   left side (prefix columns NULL, right row verbatim);
//! - multiple matches multiply rows, no match at all pads every row;
//! - WHERE on the NULL-padded column: `IS NULL` = anti-join,
//!   `IS NOT NULL` = semi-join;
//! - RIGHT JOIN ≡ the swapped LEFT JOIN (same row set);
//! - the OUTER keyword is optional in both directions;
//! - filters + aliases on outer results, chains of outer joins
//!   (`a LEFT JOIN b LEFT JOIN c`) and outer mixed with inner;
//! - a secondary index on the join key leaves results unchanged.

mod common;

use common::mysql::{col, ddl, rows, rows_ordered, s, world};
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

/// The shared fixture: 3 depts, 4 emps (two in dept 1, one in dept 2,
/// `dee` unmatched), bonuses for ada and twice for bob (multiple
/// matches), and a `ghost` table whose ids never match a dept id.
async fn setup(tag: &str) -> (common::ProcNode, mysql_async::Conn) {
    let (node, mut c) = world(tag).await;
    ddl(
        &mut c,
        "CREATE TABLE dept (id BIGINT PRIMARY KEY, name VARCHAR(32) NOT NULL)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE emp (id BIGINT PRIMARY KEY, name VARCHAR(32) NOT NULL, \
         dept_id BIGINT NULL)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE bonus (emp_id BIGINT, amount BIGINT) PRIMARY KEY(emp_id, amount)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE ghost (id BIGINT PRIMARY KEY, tag VARCHAR(8) NOT NULL)",
    )
    .await;
    c.query_drop("INSERT INTO dept (id, name) VALUES (1,'eng'),(2,'ops'),(3,'hr')")
        .await
        .expect("seed dept");
    c.query_drop(
        "INSERT INTO emp (id, name, dept_id) VALUES \
         (1,'ada',1),(2,'bob',1),(3,'cyd',2),(4,'dee',NULL)",
    )
    .await
    .expect("seed emp");
    c.query_drop("INSERT INTO bonus (emp_id, amount) VALUES (1,100),(2,200),(2,250)")
        .await
        .expect("seed bonus");
    c.query_drop("INSERT INTO ghost (id, tag) VALUES (97,'x'),(98,'y')")
        .await
        .expect("seed ghost");
    (node, c)
}

#[tokio::test]
async fn left_join_null_pads_the_right_side() {
    let (mut node, mut c) = setup("left-pad").await;
    assert_eq!(
        rows(
            &mut c,
            "SELECT e.id, e.name, d.id, d.name FROM emp e \
             LEFT JOIN dept d ON e.dept_id = d.id ORDER BY e.id"
        )
        .await,
        vec![
            vec![s("1"), s("ada"), s("1"), s("eng")],
            vec![s("2"), s("bob"), s("1"), s("eng")],
            vec![s("3"), s("cyd"), s("2"), s("ops")],
            vec![s("4"), s("dee"), MVal::NULL, MVal::NULL],
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn left_join_multiplies_matches_and_no_match_pads_all() {
    let (mut node, mut c) = setup("left-mult").await;
    // bob's two bonuses multiply his row; cyd and dee get NULL.
    assert_eq!(
        rows(
            &mut c,
            "SELECT e.name, b.amount FROM emp e LEFT JOIN bonus b ON b.emp_id = e.id \
             ORDER BY e.name, b.amount"
        )
        .await,
        vec![
            vec![s("ada"), s("100")],
            vec![s("bob"), s("200")],
            vec![s("bob"), s("250")],
            vec![s("cyd"), MVal::NULL],
            vec![s("dee"), MVal::NULL],
        ]
    );
    // No match AT ALL: every left row survives, the whole right side
    // is NULL.
    assert_eq!(
        rows(
            &mut c,
            "SELECT d.name, g.tag FROM dept d LEFT JOIN ghost g ON d.id = g.id \
             ORDER BY d.name"
        )
        .await,
        vec![
            vec![s("eng"), MVal::NULL],
            vec![s("hr"), MVal::NULL],
            vec![s("ops"), MVal::NULL],
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn where_on_null_padded_column_is_anti_and_semi_join() {
    let (mut node, mut c) = setup("anti").await;
    // IS NULL keeps exactly the left rows with no right match.
    assert_eq!(
        col(
            &mut c,
            "SELECT e.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id \
             WHERE d.id IS NULL"
        )
        .await,
        vec![s("dee")]
    );
    assert_eq!(
        rows(
            &mut c,
            "SELECT e.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id \
             WHERE d.name IS NULL"
        )
        .await,
        vec![vec![s("dee")]]
    );
    // IS NOT NULL on the padded column = semi-join.
    assert_eq!(
        rows_ordered(
            &mut c,
            "SELECT e.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id \
             WHERE d.id IS NOT NULL"
        )
        .await,
        vec![vec![s("ada")], vec![s("bob")], vec![s("cyd")]]
    );
    node.kill_now();
}

#[tokio::test]
async fn right_join_pads_left_side_and_equals_swapped_left() {
    let (mut node, mut c) = setup("right").await;
    let right = rows(
        &mut c,
        "SELECT d.id, d.name, e.id, e.name FROM emp e \
         RIGHT JOIN dept d ON e.dept_id = d.id ORDER BY d.id, e.id",
    )
    .await;
    assert_eq!(
        right,
        vec![
            vec![s("1"), s("eng"), s("1"), s("ada")],
            vec![s("1"), s("eng"), s("2"), s("bob")],
            vec![s("2"), s("ops"), s("3"), s("cyd")],
            // hr matches no emp: the LEFT side is padded with NULLs.
            vec![s("3"), s("hr"), MVal::NULL, MVal::NULL],
        ]
    );
    // RIGHT JOIN(A, B) == LEFT JOIN(B, A): same rows, same column
    // order (dept first), only the ON operands swap.
    let swapped = rows(
        &mut c,
        "SELECT d.id, d.name, e.id, e.name FROM dept d \
         LEFT JOIN emp e ON e.dept_id = d.id ORDER BY d.id, e.id",
    )
    .await;
    assert_eq!(right, swapped, "RIGHT JOIN must equal swapped LEFT JOIN");
    // Right anti-join: depts with no emp, via a padded left column.
    assert_eq!(
        rows(
            &mut c,
            "SELECT d.name FROM emp e RIGHT JOIN dept d ON e.dept_id = d.id \
             WHERE e.id IS NULL"
        )
        .await,
        vec![vec![s("hr")]]
    );
    node.kill_now();
}

#[tokio::test]
async fn outer_keyword_is_optional_in_both_directions() {
    let (mut node, mut c) = setup("outer-kw").await;
    let plain_left = rows_ordered(
        &mut c,
        "SELECT e.id, d.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id",
    )
    .await;
    let spelt_left = rows_ordered(
        &mut c,
        "SELECT e.id, d.name FROM emp e LEFT OUTER JOIN dept d ON e.dept_id = d.id",
    )
    .await;
    assert_eq!(plain_left, spelt_left, "LEFT [OUTER] JOIN is one join kind");
    let plain_right = rows_ordered(
        &mut c,
        "SELECT d.id, e.name FROM emp e RIGHT JOIN dept d ON e.dept_id = d.id",
    )
    .await;
    let spelt_right = rows_ordered(
        &mut c,
        "SELECT d.id, e.name FROM emp e RIGHT OUTER JOIN dept d ON e.dept_id = d.id",
    )
    .await;
    assert_eq!(
        plain_right, spelt_right,
        "RIGHT [OUTER] JOIN is one join kind"
    );
    node.kill_now();
}

#[tokio::test]
async fn outer_chains_filters_and_inner_mix() {
    let (mut node, mut c) = setup("chain").await;
    // Chain of outer joins: the miss in the middle (dee) and at the
    // tail (cyd, ada-with-no-bonus rows) each pad only their own side.
    assert_eq!(
        rows(
            &mut c,
            "SELECT e.name AS who, d.name AS team, b.amount AS amt \
             FROM emp e LEFT JOIN dept d ON e.dept_id = d.id \
             LEFT JOIN bonus b ON b.emp_id = e.id ORDER BY who, amt"
        )
        .await,
        vec![
            vec![s("ada"), s("eng"), s("100")],
            vec![s("bob"), s("eng"), s("200")],
            vec![s("bob"), s("eng"), s("250")],
            vec![s("cyd"), s("ops"), MVal::NULL],
            vec![s("dee"), MVal::NULL, MVal::NULL],
        ]
    );
    // Filter on the padded side turns the chain into a semi-join over
    // matched rows; aliases resolve in WHERE.
    assert_eq!(
        rows_ordered(
            &mut c,
            "SELECT e.name AS who, b.amount FROM emp e \
             LEFT JOIN dept d ON e.dept_id = d.id \
             LEFT JOIN bonus b ON b.emp_id = e.id \
             WHERE d.name = 'eng' AND b.amount >= 200"
        )
        .await,
        vec![vec![s("bob"), s("200")], vec![s("bob"), s("250")]]
    );
    // Inner join in the middle of an outer chain: 'hr' drops out via
    // the INNER, unmatched emps pad via the LEFT.
    assert_eq!(
        rows(
            &mut c,
            "SELECT d.name, e.name, b.amount FROM dept d \
             INNER JOIN emp e ON e.dept_id = d.id \
             LEFT JOIN bonus b ON b.emp_id = e.id ORDER BY d.name, e.name, b.amount"
        )
        .await,
        vec![
            vec![s("eng"), s("ada"), s("100")],
            vec![s("eng"), s("bob"), s("200")],
            vec![s("eng"), s("bob"), s("250")],
            vec![s("ops"), s("cyd"), MVal::NULL],
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn indexed_join_key_keeps_outer_join_semantics() {
    let (mut node, mut c) = setup("idx").await;
    ddl(&mut c, "CREATE INDEX idx_emp_dept ON emp (dept_id)").await;
    ddl(&mut c, "CREATE INDEX idx_bonus_emp ON bonus (emp_id)").await;
    assert_eq!(
        rows(
            &mut c,
            "SELECT e.id, e.name, d.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id \
             ORDER BY e.id"
        )
        .await,
        vec![
            vec![s("1"), s("ada"), s("eng")],
            vec![s("2"), s("bob"), s("eng")],
            vec![s("3"), s("cyd"), s("ops")],
            vec![s("4"), s("dee"), MVal::NULL],
        ]
    );
    // The anti-join pattern must survive the miss-side index too.
    assert_eq!(
        rows(
            &mut c,
            "SELECT e.name FROM emp e LEFT JOIN dept d ON e.dept_id = d.id \
             WHERE d.id IS NULL"
        )
        .await,
        vec![vec![s("dee")]]
    );
    node.kill_now();
}
