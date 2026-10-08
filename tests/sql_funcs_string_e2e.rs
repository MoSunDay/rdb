//! M1 string-family function e2e over a real rdb MySQL-protocol process
//! (`plans/2026-10-06-mysql-gap/m1-expression-functions.md`; scaffolding
//! in `tests/common/mysql.rs`): one call per landed function with
//! exact wire text, NULL propagation, REGEXP (byte-wise,
//! case-sensitive), GROUP_CONCAT shapes, functions as WHERE / GROUP BY
//! / JOIN keys, prepared parameters feeding functions, and the arity
//! negative matrix.

mod common;

use common::mysql::{
    assert_exprs, col, col_types, ddl, funcs_node, i, one, rows, s, server_error,
    ER_NOT_SUPPORTED_YET, ER_TRUNCATED_WRONG_VALUE, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
};
use mysql_async::consts::ColumnType::*;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

#[tokio::test]
async fn string_family_spot_checks() {
    let (mut node, mut c) = funcs_node("spot").await;
    assert_exprs(
        &mut c,
        &[
            ("CONCAT('a', 'b', 'c')", s("abc")),
            ("CONCAT('x=', 42)", s("x=42")),
            ("CONCAT_WS(',', 'a', 'b')", s("a,b")),
            ("CONCAT_WS('-', 'a', NULL, 'b')", s("a-b")),
            // Empty strings are real arguments: their separators stay
            // (P0: the old gate dropped them and yielded "b"/"a,b").
            ("CONCAT_WS(',', '', 'b')", s(",b")),
            ("CONCAT_WS(',', 'a', '', 'b')", s("a,,b")),
            ("CONCAT_WS(',', '', '')", s(",")),
            ("SUBSTRING('Quadratically', 5)", s("ratically")),
            ("SUBSTRING('Quadratically', 5, 6)", s("ratica")),
            ("SUBSTRING('Quadratically', -5)", s("cally")),
            ("SUBSTRING('Quadratically', -5, 3)", s("cal")),
            ("SUBSTRING('Quadratically' FROM 5 FOR 6)", s("ratica")),
            ("SUBSTR('Quadratically', 5)", s("ratically")),
            ("LEFT('foobarbar', 4)", s("foob")),
            ("RIGHT('foobarbar', 4)", s("rbar")),
            ("LPAD('hi', 4, '??')", s("??hi")),
            ("RPAD('hi', 5, '?')", s("hi???")),
            ("LPAD('hello', 2, '?')", s("he")),
            ("REPEAT('ab', 3)", s("ababab")),
            ("REPEAT('ab', 0)", s("")),
            ("LOCATE('bar', 'foobarbar')", i(4)),
            ("LOCATE('bar', 'foobarbar', 5)", i(7)),
            ("INSTR('foobarbar', 'bar')", i(4)),
            ("POSITION('bar' IN 'foobarbar')", i(4)),
            ("REPLACE('www.mysql.com', 'mysql', 'rdb')", s("www.rdb.com")),
            ("REPLACE('aaa', 'a', '')", s("")),
            ("TRIM('  bar  ')", s("bar")),
            ("TRIM(BOTH 'x' FROM 'xxxbarxxx')", s("bar")),
            ("TRIM(LEADING 'x' FROM 'xxxbarxxx')", s("barxxx")),
            ("TRIM(TRAILING 'x' FROM 'xxxbarxxx')", s("xxxbar")),
            // Keyword with no remstr: remstr defaults to one space.
            ("TRIM(BOTH FROM '  bar  ')", s("bar")),
            ("TRIM(LEADING FROM '  bar  ')", s("bar  ")),
            ("TRIM(TRAILING FROM '  bar  ')", s("  bar")),
            ("REVERSE('abc')", s("cba")),
            ("HEX('abc')", s("616263")),
            ("HEX(255)", s("00000000000000FF")),
            ("UNHEX('4D7953514C')", s("MySQL")),
            ("UNHEX('zz')", MVal::NULL),
            ("UPPER('hej')", s("HEJ")),
            ("LOWER('HEJ')", s("hej")),
            ("LENGTH('hé')", i(3)),
            ("CHAR_LENGTH('hé')", i(2)),
        ],
    )
    .await;
    // String answers are VAR_STRING columns; UNHEX answers a binary
    // string (BLOB), even though the text cell is the raw bytes.
    assert_eq!(
        col_types(
            &mut c,
            "SELECT CONCAT('a', 'b'), CHAR_LENGTH('ab'), UNHEX('41')"
        )
        .await,
        vec![MYSQL_TYPE_VAR_STRING, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_BLOB]
    );
    node.kill_now();
}

#[tokio::test]
async fn nulls_propagate_through_string_functions() {
    let (mut node, mut c) = funcs_node("null").await;
    assert_exprs(
        &mut c,
        &[
            ("CONCAT('a', NULL)", MVal::NULL),
            ("CONCAT_WS(NULL, 'a', 'b')", MVal::NULL),
            ("SUBSTRING('abc', NULL)", MVal::NULL),
            ("LEFT('abc', NULL)", MVal::NULL),
            ("LPAD(NULL, 4, '?')", MVal::NULL),
            ("REPEAT('a', NULL)", MVal::NULL),
            ("LOCATE(NULL, 'abc')", MVal::NULL),
            ("REPLACE('a', NULL, 'b')", MVal::NULL),
            ("TRIM(NULL)", MVal::NULL),
            ("REVERSE(NULL)", MVal::NULL),
            ("HEX(NULL)", MVal::NULL),
            ("UPPER(NULL)", MVal::NULL),
            ("LENGTH(NULL)", MVal::NULL),
        ],
    )
    .await;
    // A NULL column operand propagates through REGEXP the same way.
    let got = one(&mut c, "SELECT name REGEXP 'a' FROM f WHERE id = 3").await;
    assert_eq!(got, MVal::NULL);
    // Columns carry the same rule: a NULL name stays NULL through the
    // whole projection pipeline, not just the literal path.
    let got = rows(
        &mut c,
        "SELECT CONCAT(name, '!'), UPPER(name), CHAR_LENGTH(name) FROM f WHERE id = 3",
    )
    .await;
    assert_eq!(got, vec![vec![MVal::NULL, MVal::NULL, MVal::NULL]]);
    node.kill_now();
}

#[tokio::test]
async fn regexp_is_byte_wise_and_case_sensitive() {
    let (mut node, mut c) = funcs_node("regexp").await;
    assert_exprs(
        &mut c,
        &[
            ("'hello' REGEXP '^h.*o$'", i(1)),
            ("'Hello' REGEXP '^h'", i(0)),
            ("'Hello' REGEXP '^H'", i(1)),
            ("'abc' RLIKE 'b'", i(1)),
            ("'abc' RLIKE 'B'", i(0)),
            ("'abc' NOT REGEXP 'B'", i(1)),
            ("'abc' NOT REGEXP 'b'", i(0)),
        ],
    )
    .await;
    // REGEXP answers 1/0 on a LONGLONG column (MySQL wire shape).
    assert_eq!(
        col_types(&mut c, "SELECT 'a' REGEXP 'a'").await,
        vec![MYSQL_TYPE_LONGLONG]
    );
    // A pattern the regex engine rejects is a loud wrong-value error,
    // never a silent mismatch.
    let e = server_error(&mut c, "SELECT 'a' REGEXP '('").await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE, "{}", e.message);
    assert!(e.message.to_lowercase().contains("regexp"), "{}", e.message);
    node.kill_now();
}

#[tokio::test]
async fn functions_in_where_group_by_and_join() {
    let (mut node, mut c) = funcs_node("clauses").await;
    ddl(
        &mut c,
        "CREATE TABLE j (id BIGINT PRIMARY KEY, tag VARCHAR(32))",
    )
    .await;
    c.query_drop("INSERT INTO j (id, tag) VALUES (1, 'ADA'), (2, 'BOB')")
        .await
        .expect("seed j");

    // WHERE over a function of the row (case-sensitive UPPER: only
    // 'ada' matches 'ADA'; 'Bob' does not match 'BOB').
    let got = col(&mut c, "SELECT id FROM f WHERE UPPER(name) = 'ADA'").await;
    assert_eq!(got, vec![i(1)]);
    let got = col(&mut c, "SELECT id FROM f WHERE CHAR_LENGTH(name) = 3").await;
    assert_eq!(got, vec![i(1), i(2)]);

    // GROUP BY over a function key; ORDER BY rides the projection
    // alias (M0 semantics).
    let got = rows(
        &mut c,
        "SELECT UPPER(name) AS u, COUNT(*) AS n FROM f WHERE name IS NOT NULL \
         GROUP BY UPPER(name) ORDER BY u",
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![s("ADA"), i(1)],
            vec![s("BOB"), i(1)],
            vec![s("CAROL"), i(1)]
        ]
    );

    // ORDER BY LENGTH(name) sorts by the computed length (3, 3, 5).
    let got = col(
        &mut c,
        "SELECT name FROM f WHERE name IS NOT NULL ORDER BY LENGTH(name), name",
    )
    .await;
    assert_eq!(got, vec![s("Bob"), s("ada"), s("carol")]);

    // A function in the JOIN condition resolves columns from both sides.
    let got = col(
        &mut c,
        "SELECT f.id FROM f JOIN j ON UPPER(f.name) = j.tag ORDER BY f.id",
    )
    .await;
    assert_eq!(got, vec![i(1), i(2)]);
    node.kill_now();
}

#[tokio::test]
async fn prepared_params_feed_string_functions() {
    let (mut node, mut c) = funcs_node("prep").await;

    // Function-of-column vs a bound parameter.
    let stmt = c
        .prep("SELECT id FROM f WHERE UPPER(name) = ? ORDER BY id")
        .await
        .expect("prep upper");
    let got: Vec<(i64,)> = c.exec(&stmt, ("BOB",)).await.expect("exec upper");
    assert_eq!(got, vec![(2,)]);

    // Parameter as the LOCATE needle (byte-wise: 'Bo' hits, 'bo' does not).
    let stmt = c
        .prep("SELECT id FROM f WHERE LOCATE(?, name) > 0")
        .await
        .expect("prep locate");
    let got: Vec<(i64,)> = c.exec(&stmt, ("Bo",)).await.expect("exec locate");
    assert_eq!(got, vec![(2,)]);
    let got: Vec<(i64,)> = c.exec(&stmt, ("bo",)).await.expect("exec locate miss");
    assert!(got.is_empty());

    // Parameter inside a projection function and a bound LIMIT (M0).
    let stmt = c
        .prep("SELECT CONCAT(name, '!') FROM f WHERE id = ? LIMIT ?")
        .await
        .expect("prep concat");
    let got: Vec<(String,)> = c.exec(&stmt, (1i64, 5i64)).await.expect("exec concat");
    assert_eq!(got, vec![("ada!".to_string(),)]);
    node.kill_now();
}

#[tokio::test]
async fn group_concat_shapes() {
    let (mut node, mut c) = funcs_node("gc").await;
    ddl(
        &mut c,
        "CREATE TABLE g (id BIGINT PRIMARY KEY, grp BIGINT, tag VARCHAR(16))",
    )
    .await;
    c.query_drop(
        "INSERT INTO g (id, grp, tag) VALUES \
         (1, 1, 'a'), (2, 1, 'b'), (3, 1, NULL), (4, 2, 'a'), (5, 2, 'a'), (6, 3, NULL)",
    )
    .await
    .expect("seed g");

    // Whole-table: default separator, NULLs skipped, scan (id) order.
    let got = one(&mut c, "SELECT GROUP_CONCAT(tag) FROM g").await;
    assert_eq!(got, s("a,b,a,a"), "NULLs skipped, default ','");
    // Numbers render in their canonical text.
    let got = one(&mut c, "SELECT GROUP_CONCAT(id) FROM g WHERE grp = 1").await;
    assert_eq!(got, s("1,2,3"));
    // DISTINCT dedups.
    let got = one(&mut c, "SELECT GROUP_CONCAT(DISTINCT tag) FROM g").await;
    assert_eq!(got, s("a,b"));
    // Custom separator.
    let got = one(&mut c, "SELECT GROUP_CONCAT(tag SEPARATOR '; ') FROM g").await;
    assert_eq!(got, s("a; b; a; a"));

    // Per-group with an explicit GROUP BY: grp 3 has only NULL tags and
    // aggregates to the empty join of zero kept values.
    let got = rows(
        &mut c,
        "SELECT grp, GROUP_CONCAT(tag SEPARATOR '|') FROM g GROUP BY grp ORDER BY grp",
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![i(1), s("a|b")],
            vec![i(2), s("a|a")],
            vec![i(3), s("")]
        ]
    );
    // Multiple arguments concatenate per row (the CONCAT wrap).
    let got = one(
        &mut c,
        "SELECT GROUP_CONCAT(id, '=', tag) FROM g WHERE grp = 2",
    )
    .await;
    assert_eq!(got, s("4=a,5=a"));
    // GROUP_CONCAT is a string column regardless of argument type.
    assert_eq!(
        col_types(&mut c, "SELECT GROUP_CONCAT(id) FROM g").await,
        vec![MYSQL_TYPE_VAR_STRING]
    );

    // Inner ORDER BY is the loud P2 reject (unordered groups in v1).
    let e = server_error(&mut c, "SELECT GROUP_CONCAT(tag ORDER BY tag) FROM g").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    assert!(
        e.message.to_lowercase().contains("group_concat"),
        "{}",
        e.message
    );
    node.kill_now();
}

#[tokio::test]
async fn string_negative_matrix() {
    let (mut node, mut c) = funcs_node("neg").await;

    // Known names with a bad parameter count fail at prepare (1582).
    for sql in [
        "SELECT UPPER('a', 'b')",
        "SELECT REPLACE('a', 'b')",
        "SELECT ROUND(1, 2, 3)",
        "SELECT LOCATE('a')",
    ] {
        let e = match c.prep(sql).await {
            Err(mysql_async::Error::Server(e)) => e,
            other => panic!("expected prepare error for {sql}, got {other:?}"),
        };
        assert_eq!(
            e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
            "{sql}: {}",
            e.message
        );
        assert!(
            e.message.contains("Incorrect parameter count"),
            "{sql}: {}",
            e.message
        );
    }
    // LPAD below its minimum arity is the same 1582 at prepare (the
    // evaluator's TRIM 2-arg reject is unreachable through the SQL
    // grammar -- the special form does not spell it; unit-pinned).
    let e = match c.prep("SELECT LPAD('a', 1)").await {
        Err(mysql_async::Error::Server(e)) => e,
        other => panic!("expected prepare error for LPAD, got {other:?}"),
    };
    assert_eq!(e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT, "{}", e.message);
    // GROUP_CONCAT with no argument is the native-function arity
    // error (MySQL 1582), same as the other aggregates.
    let e = server_error(&mut c, "SELECT GROUP_CONCAT()").await;
    assert_eq!(e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT, "{}", e.message);
    // Aggregate wrong-arity is 1582 too (MySQL's native-function
    // code), not the 1235 unsupported shape.
    for sql in [
        "SELECT SUM(1, 2)",
        "SELECT COUNT(1, 2)",
        "SELECT AVG(1, 2)",
        "SELECT MIN()",
        "SELECT MAX()",
        "SELECT SUM()",
    ] {
        let e = match c.prep(sql).await {
            Err(mysql_async::Error::Server(e)) => e,
            other => panic!("expected prepare error for {sql}, got {other:?}"),
        };
        assert_eq!(
            e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
            "{sql}: {}",
            e.message
        );
    }
    // Unknown function names stay loud (1235), never NULL.
    let e = server_error(&mut c, "SELECT NO_SUCH_FN(1)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    assert!(
        e.message.to_lowercase().contains("unknown function"),
        "{}",
        e.message
    );
    node.kill_now();
}

#[tokio::test]
async fn oversized_string_results_are_null() {
    // REPEAT/LPAD/RPAD cap the RESULT at 16 MiB (the max MySQL wire
    // packet): a request past the cap is NULL (like MySQL against
    // max_allowed_packet), never an unbounded allocation.
    let (mut node, mut c) = funcs_node("caps").await;
    assert_exprs(
        &mut c,
        &[
            ("REPEAT('x', 1073741824)", MVal::NULL),
            ("LPAD('x', 1073741824, '?')", MVal::NULL),
            ("RPAD('x', 1073741824, '?')", MVal::NULL),
            // Small shapes keep their answers.
            ("REPEAT('x', 3)", s("xxx")),
        ],
    )
    .await;
    node.kill_now();
}
