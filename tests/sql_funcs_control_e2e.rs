//! M1 control-family function e2e over a real rdb MySQL-protocol process
//! (`plans/2026-10-06-mysql-gap/m1-expression-functions.md`; scaffolding
//! in `tests/common/mysql.rs`): CASE in both forms (lazy THENs,
//! NULL-never-matches), CAST/CONVERT targets and their rejections, the
//! lazy IF/IFNULL/NULLIF/COALESCE quartet, VERSION, the three-valued
//! operator layer (`<=>` vs `=` with NULLs, XOR truth table), cross-
//! family result metadata, prepared parameters feeding the control
//! forms, and the negative matrix.

mod common;

use common::mysql::{
    assert_exprs, col, col_types, funcs_node, i, one, rows, s, server_error, ER_NOT_SUPPORTED_YET,
    ER_TRUNCATED_WRONG_VALUE, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
};
use mysql_async::consts::ColumnType::*;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

#[tokio::test]
async fn case_both_forms_and_lazy_thens() {
    let (mut node, mut c) = funcs_node("case").await;
    assert_exprs(
        &mut c,
        &[
            // Searched CASE: first true WHEN wins; ELSE is the default.
            (
                "CASE WHEN 1 = 0 THEN 'a' WHEN 1 = 1 THEN 'b' ELSE 'c' END",
                s("b"),
            ),
            ("CASE WHEN 1 = 0 THEN 'a' ELSE 'c' END", s("c")),
            // WHEN NULL is unknown, not true: the ELSE answers.
            ("CASE WHEN NULL THEN 1 ELSE 2 END", i(2)),
            // No true WHEN and no ELSE -> NULL.
            ("CASE WHEN 1 = 0 THEN 'a' END", MVal::NULL),
            // Simple CASE compares with `=`: NULL never matches, not
            // even WHEN NULL.
            ("CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END", s("two")),
            ("CASE 2 WHEN 1 THEN 'one' END", MVal::NULL),
            ("CASE NULL WHEN NULL THEN 1 ELSE 2 END", i(2)),
        ],
    )
    .await;
    // The matching THEN is the only branch evaluated: the erroring
    // untaken branches below never run (lazy short-circuit).
    assert_exprs(
        &mut c,
        &[
            ("CASE WHEN 1 = 1 THEN 'ok' ELSE ROUND('abc') END", s("ok")),
            ("CASE WHEN 1 = 0 THEN ROUND('abc') ELSE 7 END", i(7)),
        ],
    )
    .await;
    // CASE over a row column; metadata types as the first THEN.
    let got = one(
        &mut c,
        "SELECT CASE WHEN id = 2 THEN 'Bob?' ELSE name END FROM f WHERE id = 2",
    )
    .await;
    assert_eq!(got, s("Bob?"));
    assert_eq!(
        col_types(
            &mut c,
            "SELECT CASE WHEN 1 = 1 THEN 1 ELSE 2 END, CASE WHEN 1 = 1 THEN 'a' ELSE 'b' END"
        )
        .await,
        vec![MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING]
    );
    node.kill_now();
}

#[tokio::test]
async fn lazy_control_functions() {
    let (mut node, mut c) = funcs_node("lazy").await;
    assert_exprs(
        &mut c,
        &[
            ("IF(1 > 0, 'yes', 'no')", s("yes")),
            ("IF(0, 'yes', 'no')", s("no")),
            // A NULL condition is unknown -> the ELSE branch.
            ("IF(NULL, 1, 2)", i(2)),
            ("IFNULL(NULL, 1)", i(1)),
            ("IFNULL(7, 9)", i(7)),
            ("IFNULL(NULL, NULL)", MVal::NULL),
            ("NULLIF(2, 2)", MVal::NULL),
            ("NULLIF(2, 3)", i(2)),
            // Plain `=` inside: a NULL first argument stays NULL.
            ("NULLIF(NULL, 1)", MVal::NULL),
            ("COALESCE(NULL, NULL, 3)", i(3)),
            ("COALESCE(NULL, 'x', 1)", s("x")),
            ("COALESCE(NULL, NULL)", MVal::NULL),
            ("VERSION()", s(env!("CARGO_PKG_VERSION"))),
        ],
    )
    .await;
    // The untaken branch is never evaluated (an erroring function
    // there proves laziness better than any value).
    assert_exprs(
        &mut c,
        &[
            ("IF(0, ROUND('abc'), 7)", i(7)),
            ("IFNULL(7, ROUND('abc'))", i(7)),
            ("NULLIF(NULL, ROUND('abc'))", MVal::NULL),
            ("COALESCE(1, ROUND('abc'))", i(1)),
        ],
    )
    .await;
    // Row-bound: NULL names answer the fallback.
    let got = col(&mut c, "SELECT IFNULL(name, '<anon>') FROM f ORDER BY id").await;
    assert_eq!(got, vec![s("ada"), s("Bob"), s("<anon>"), s("carol")]);
    node.kill_now();
}

#[tokio::test]
async fn cast_and_convert_targets() {
    let (mut node, mut c) = funcs_node("cast").await;
    assert_exprs(
        &mut c,
        &[
            ("CAST(42 AS CHAR)", s("42")),
            ("CAST(' 42 ' AS SIGNED)", i(42)),
            // Decimal-ish strings round half away from zero on the
            // integer path, like doubles and decimals do.
            ("CAST('12.5' AS SIGNED)", i(13)),
            ("CAST(-12.5 AS SIGNED)", i(-13)),
            ("CAST(1.5 AS SIGNED)", i(2)),
            ("CAST('héllo' AS CHAR(2))", s("hé")),
            ("CAST(999 AS DECIMAL(5, 2))", s("999.00")),
            ("CAST(NULL AS SIGNED)", MVal::NULL),
            ("CONVERT(42, CHAR)", s("42")),
            ("CONVERT('12.5', SIGNED)", i(13)),
            ("CONVERT('x' USING utf8)", s("x")),
        ],
    )
    .await;
    // Junk strings and unsigned underflow are loud.
    let e = server_error(&mut c, "SELECT CAST('abc' AS SIGNED)").await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE, "{}", e.message);
    assert!(
        e.message.to_lowercase().contains("incorrect integer"),
        "{}",
        e.message
    );
    let e = server_error(&mut c, "SELECT CAST(-1 AS UNSIGNED)").await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE, "{}", e.message);
    assert!(
        e.message.to_uppercase().contains("UNSIGNED"),
        "{}",
        e.message
    );

    // Result metadata: CHAR -> text, SIGNED -> BIGINT, DECIMAL keeps
    // the declared shape.
    assert_eq!(
        col_types(
            &mut c,
            "SELECT CAST(id AS CHAR), CAST(id AS SIGNED), CAST(price AS DECIMAL(6, 2)) FROM f"
        )
        .await,
        vec![
            MYSQL_TYPE_VAR_STRING,
            MYSQL_TYPE_LONGLONG,
            MYSQL_TYPE_NEWDECIMAL
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn null_safe_eq_and_xor_truth_tables() {
    let (mut node, mut c) = funcs_node("ops").await;
    assert_exprs(
        &mut c,
        &[
            // Plain `=` is three-valued: NULL on either side is NULL.
            ("NULL = NULL", MVal::NULL),
            ("1 = NULL", MVal::NULL),
            ("1 = 1", i(1)),
            // `<=>` never yields NULL: NULL <=> NULL is true.
            ("NULL <=> NULL", i(1)),
            ("NULL <=> 1", i(0)),
            ("1 <=> NULL", i(0)),
            ("1 <=> 1", i(1)),
            ("1 <=> 2", i(0)),
            // XOR is three-valued logical (exclusive or).
            ("1 XOR 1", i(0)),
            ("1 XOR 0", i(1)),
            ("0 XOR 0", i(0)),
            ("1 XOR NULL", MVal::NULL),
            ("NULL XOR NULL", MVal::NULL),
        ],
    )
    .await;
    // Both answer 0/1 on TINY columns (MySQL boolean shape).
    assert_eq!(
        col_types(&mut c, "SELECT 1 <=> 1, NULL = NULL, 1 XOR 1").await,
        vec![MYSQL_TYPE_TINY, MYSQL_TYPE_TINY, MYSQL_TYPE_TINY]
    );
    // Row-bound: `<=>` is how NULL equality is spelled in predicates.
    let got = col(&mut c, "SELECT id FROM f WHERE name <=> NULL").await;
    assert_eq!(got, vec![i(3)]);
    node.kill_now();
}

#[tokio::test]
async fn cross_family_result_metadata() {
    let (mut node, mut c) = funcs_node("meta").await;
    // One projection across families: numeric stays numeric, string
    // functions stay text, extraction is BIGINT.
    assert_eq!(
        col_types(
            &mut c,
            "SELECT ROUND(price, 2), CONCAT(name, '!'), YEAR(d), CAST(id AS CHAR), \
             CHAR_LENGTH(name), IFNULL(name, 'x'), DATE_FORMAT(ts, '%Y') FROM f"
        )
        .await,
        vec![
            MYSQL_TYPE_NEWDECIMAL,
            MYSQL_TYPE_VAR_STRING,
            MYSQL_TYPE_LONGLONG,
            MYSQL_TYPE_VAR_STRING,
            MYSQL_TYPE_LONGLONG,
            MYSQL_TYPE_VAR_STRING,
            MYSQL_TYPE_VAR_STRING,
        ]
    );
    // And the cells parse as their domains promise.
    let got = rows(
        &mut c,
        "SELECT ROUND(price, 2), CONCAT(name, '!'), YEAR(d) FROM f WHERE id = 1",
    )
    .await;
    assert_eq!(got, vec![vec![s("10.76"), s("ada!"), i(2024)]]);
    node.kill_now();
}

#[tokio::test]
async fn prepared_params_feed_control_functions() {
    let (mut node, mut c) = funcs_node("prep").await;

    // The M0 baseline shape: a bound key predicate.
    let stmt = c
        .prep("SELECT id FROM f WHERE id = ?")
        .await
        .expect("prep id");
    let got: Vec<(i64,)> = c.exec(&stmt, (2i64,)).await.expect("exec id");
    assert_eq!(got, vec![(2,)]);

    // A parameter as the IFNULL fallback (NULL name row).
    let stmt = c
        .prep("SELECT IFNULL(name, ?) FROM f WHERE id = ?")
        .await
        .expect("prep ifnull");
    let got: Vec<(String,)> = c
        .exec(&stmt, ("anon".to_string(), 3i64))
        .await
        .expect("exec ifnull");
    assert_eq!(got, vec![("anon".to_string(),)]);

    // Parameters inside CASE branches plus a bound LIMIT (M0).
    let stmt = c
        .prep("SELECT CASE WHEN id = ? THEN 'hit' ELSE 'miss' END FROM f ORDER BY id LIMIT ?")
        .await
        .expect("prep case");
    let got: Vec<(String,)> = c.exec(&stmt, (1i64, 2i64)).await.expect("exec case");
    assert_eq!(got, vec![("hit".to_string(),), ("miss".to_string(),)]);

    // COALESCE over a bound parameter. The projection types as text
    // (a placeholder's type is unknown until bind): a text bind passes
    // through, and numeric binds coerce compatibly per the announced
    // column type (fixed 2026-10-08, conv_bin) -- i64/f64 against the
    // VAR_STRING column ship as their canonical text.
    let stmt = c
        .prep("SELECT COALESCE(NULL, ?)")
        .await
        .expect("prep coalesce");
    let got: Vec<(String,)> = c
        .exec(&stmt, ("nine".to_string(),))
        .await
        .expect("exec coalesce");
    assert_eq!(got, vec![("nine".to_string(),)]);
    let got: Vec<(String,)> = c.exec(&stmt, (42i64,)).await.expect("coalesce i64 bind");
    assert_eq!(got, vec![("42".to_string(),)]);
    let got: Vec<(String,)> = c.exec(&stmt, (1.5f64,)).await.expect("coalesce f64 bind");
    assert_eq!(got, vec![("1.5".to_string(),)]);
    node.kill_now();
}

#[tokio::test]
async fn control_negative_matrix() {
    let (mut node, mut c) = funcs_node("neg").await;

    // Bad arity fails at prepare (1582).
    for sql in [
        "SELECT IF(1, 2)",
        "SELECT IFNULL(1)",
        "SELECT NULLIF(1)",
        "SELECT COALESCE()",
        "SELECT VERSION(1)",
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
    // CAST/CONVERT targets outside the v1 subset reject loudly.
    let e = server_error(&mut c, "SELECT CAST(1 AS BINARY)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    let e = server_error(&mut c, "SELECT CONVERT('a' USING latin1)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    assert!(e.message.to_uppercase().contains("UTF8"), "{}", e.message);
    node.kill_now();
}
