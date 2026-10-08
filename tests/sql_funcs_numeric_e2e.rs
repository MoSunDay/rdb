//! M1 numeric-family function e2e over a real rdb MySQL-protocol process
//! (`plans/2026-10-06-mysql-gap/m1-expression-functions.md`; scaffolding
//! in `tests/common/mysql.rs`): exact-text spot checks, DECIMAL
//! precision on literals and columns (ROUND half-away-from-zero,
//! negative-d scaling, TRUNCATE toward zero, CEIL/FLOOR scale drop),
//! GREATEST/LEAST typing + NULL rule, u64 bitwise operators, functions
//! as WHERE / GROUP BY keys, prepared parameters, and the loud
//! rejections (string operands, mixed-type extremes, bad arity).

mod common;

use common::mysql::{
    assert_exprs, col, col_types, funcs_node, i, rows, s, server_error, ER_NOT_SUPPORTED_YET,
    ER_TRUNCATED_WRONG_VALUE, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
};
use mysql_async::consts::ColumnType::*;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

#[tokio::test]
async fn numeric_family_spot_checks() {
    let (mut node, mut c) = funcs_node("spot").await;
    assert_exprs(
        &mut c,
        &[
            ("ABS(-5)", i(5)),
            ("ROUND(2.005, 2)", s("2.01")),
            ("ROUND(2.5)", s("3")),
            ("ROUND(-2.5)", s("-3")),
            ("ROUND(1.234, 2)", s("1.23")),
            ("ROUND(125, -1)", i(130)),
            ("ROUND(-125, -1)", i(-130)),
            ("ROUND(15.00, -1)", s("20")),
            ("ROUND(-15.00, -1)", s("-20")),
            ("ROUND(NULL, 2)", MVal::NULL),
            // A NULL scale/count slot is NULL too, never a coercion error.
            ("ROUND(2.5, NULL)", MVal::NULL),
            ("TRUNCATE(2.5, NULL)", MVal::NULL),
            // CEILING answers a scale-0 exact decimal.
            ("CEILING(-1.23)", s("-1")),
            // Widening: an Int winner over decimal args re-renders at
            // the group's coarsest decimal scale (5 -> 5.00).
            ("GREATEST(5, 2.10)", s("5.00")),
            ("TRUNCATE(1.999, 1)", s("1.9")),
            ("TRUNCATE(-1.999, 1)", s("-1.9")),
            ("TRUNCATE(122, -2)", i(100)),
            ("MOD(7, 3)", i(1)),
            ("MOD(-7, 3)", i(-1)),
            ("MOD(7, 0)", MVal::NULL),
            ("MOD(12.3, 2)", s("0.3")),
            ("POW(2, 10)", s("1024")),
            ("POWER(2, 10)", s("1024")),
            // Exponents past the +/-30 bound are NULL (the double
            // answer is not trustworthy that far out); the bound
            // itself computes.
            ("POW(2, 30)", s("1073741824")),
            ("POW(2, 10000)", MVal::NULL),
            ("POW(2, -10000)", MVal::NULL),
            ("SQRT(9)", s("3")),
            ("SQRT(-9)", MVal::NULL),
            ("SIGN(-42)", i(-1)),
            ("SIGN(0)", i(0)),
            ("SIGN(2.5)", i(1)),
            ("SIGN(NULL)", MVal::NULL),
            ("GREATEST(2, 10, 3)", i(10)),
            ("LEAST(4, 2, 8)", i(2)),
            // Fractional literals are exact decimals, so an Int winner
            // widens to the coarsest decimal scale (5 -> 5.0), and the
            // decimal winner keeps its own scale.
            ("LEAST(5, 2.5)", s("2.5")),
            ("GREATEST(5, 2.5)", s("5.0")),
            // All-text extremes compare byte-wise (case-sensitive, no
            // collation -- the documented deviation).
            ("GREATEST('a', 'B')", s("a")),
            ("LEAST('a', 'B')", s("B")),
        ],
    )
    .await;
    // POW/SQRT answer DOUBLE; GREATEST/LEAST widen across their
    // argument types (decimal literals -> NEWDECIMAL, a DOUBLE column
    // -> DOUBLE, all-Int -> LONGLONG).
    assert_eq!(
        col_types(
            &mut c,
            "SELECT POW(2, 2), SQRT(4), LEAST(5, 2.5), GREATEST(1, 2), GREATEST(ratio, 0.5) FROM f"
        )
        .await,
        vec![
            MYSQL_TYPE_DOUBLE,
            MYSQL_TYPE_DOUBLE,
            MYSQL_TYPE_NEWDECIMAL,
            MYSQL_TYPE_LONGLONG,
            MYSQL_TYPE_DOUBLE,
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn decimal_precision_on_columns_and_literals() {
    let (mut node, mut c) = funcs_node("dec").await;

    // price is DECIMAL(10,3): 10.755, -2.005, NULL, 0.125. Exact
    // half-away rounding and truncate-toward-zero on the column.
    let got = rows(
        &mut c,
        "SELECT ROUND(price, 2), TRUNCATE(price, 1), ROUND(price, 0), CEILING(price) \
         FROM f WHERE price IS NOT NULL ORDER BY id",
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![s("10.76"), s("10.7"), s("11"), s("11")],
            vec![s("-2.01"), s("-2.0"), s("-2"), s("-2")],
            vec![s("0.13"), s("0.1"), s("0"), s("1")],
        ]
    );
    // A DOUBLE column rides f64 rounding instead.
    let got = rows(&mut c, "SELECT ROUND(ratio, 1) FROM f WHERE id = 4").await;
    assert_eq!(got, vec![vec![s("2.5")]]);

    // Result metadata: rounding keeps NEWDECIMAL for decimal inputs,
    // DOUBLE for double inputs.
    assert_eq!(
        col_types(
            &mut c,
            "SELECT ROUND(price, 2), TRUNCATE(price, 1), ROUND(ratio, 1) FROM f"
        )
        .await,
        vec![
            MYSQL_TYPE_NEWDECIMAL,
            MYSQL_TYPE_NEWDECIMAL,
            MYSQL_TYPE_DOUBLE,
        ]
    );
    node.kill_now();
}

/// KNOWN GAP (parse side, pinned by this e2e): sqlparser parses the
/// `CEIL`/`FLOOR` keyword spellings into dedicated `Expr::Ceil`/
/// `Expr::Floor` nodes that the translator has no branch for, so they
/// reject with 1235 even though the evaluator family is implemented
/// and unit-tested (`exec/func/numeric_tests.rs`). The `CEILING`
/// spelling is NOT a sqlparser keyword, so it rides the ordinary
/// function path and works. Follow-up: a `func_forms` translation for
/// the two keyword forms; this test then flips to value assertions.
#[tokio::test]
async fn ceil_and_floor_keyword_forms_vs_ceiling() {
    let (mut node, mut c) = funcs_node("kwceil").await;
    assert_exprs(
        &mut c,
        &[("CEILING(1.23)", s("2")), ("CEILING(2.005)", s("3"))],
    )
    .await;
    assert_exprs(
        &mut c,
        &[
            ("CEIL(1.2)", s("2")),
            ("CEIL(2.005)", s("3")),
            ("FLOOR(1.8)", s("1")),
            ("FLOOR(-2.5)", s("-3")),
        ],
    )
    .await;
    // `CEIL(x TO <unit>)` interval scaling is still a loud reject.
    for sql in ["SELECT CEIL(NOW() TO DAY)", "SELECT FLOOR(1.2 TO HOUR)"] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
    }
    node.kill_now();
}

#[tokio::test]
async fn greatest_least_null_rule_and_widening() {
    let (mut node, mut c) = funcs_node("extreme").await;
    // Any NULL operand is NULL (three-valued, both directions).
    assert_exprs(
        &mut c,
        &[
            ("GREATEST(2, NULL, 3)", MVal::NULL),
            ("LEAST(NULL, 1)", MVal::NULL),
        ],
    )
    .await;
    // Per-row over a column with a NULL price.
    let got = rows(&mut c, "SELECT id, GREATEST(price, 0.5) FROM f ORDER BY id").await;
    assert_eq!(
        got,
        vec![
            vec![i(1), s("10.755")],
            vec![i(2), s("0.5")],
            vec![i(3), MVal::NULL],
            vec![i(4), s("0.5")],
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn bitwise_operators_are_u64() {
    let (mut node, mut c) = funcs_node("bits").await;
    assert_exprs(
        &mut c,
        &[
            ("5 & 3", i(1)),
            ("5 | 3", i(7)),
            ("5 ^ 3", i(6)),
            ("1 << 3", i(8)),
            ("1024 >> 3", i(128)),
            // Two's-complement u64 semantics: 1 << 63 wraps into
            // i64::MIN, -1 is 0xFFFF.. and shifts/bits use that word.
            ("1 << 63", i(-9223372036854775808)),
            ("-1 >> 1", i(9223372036854775807)),
            ("-1 & 255", i(255)),
            // MySQL does not mask the shift count: >= 64 shifts out.
            ("1 << 64", i(0)),
            ("-1 >> 100", i(0)),
            ("NULL & 1", MVal::NULL),
        ],
    )
    .await;
    // Bitwise answers are 64-bit LONGLONG columns.
    assert_eq!(
        col_types(&mut c, "SELECT 1 & 1, 1 << 2").await,
        vec![MYSQL_TYPE_LONGLONG, MYSQL_TYPE_LONGLONG]
    );
    // Usable as a row predicate: odd ids.
    let got = col(&mut c, "SELECT id FROM f WHERE id & 1 = 1 ORDER BY id").await;
    assert_eq!(got, vec![i(1), i(3)]);
    node.kill_now();
}

#[tokio::test]
async fn numeric_functions_in_where_and_group_by() {
    let (mut node, mut c) = funcs_node("keys").await;
    // WHERE over arithmetic-shaped functions of the row.
    let got = col(&mut c, "SELECT id FROM f WHERE MOD(id, 2) = 0 ORDER BY id").await;
    assert_eq!(got, vec![i(2), i(4)]);
    let got = col(&mut c, "SELECT id FROM f WHERE ABS(id - 5) > 2 ORDER BY id").await;
    assert_eq!(got, vec![i(1), i(2)]);

    // GROUP BY over a computed sign key.
    let got = rows(
        &mut c,
        "SELECT SIGN(price) AS sg, COUNT(*) AS n FROM f WHERE price IS NOT NULL \
         GROUP BY SIGN(price) ORDER BY sg",
    )
    .await;
    assert_eq!(got, vec![vec![i(-1), i(1)], vec![i(1), i(2)]]);
    node.kill_now();
}

#[tokio::test]
async fn prepared_params_feed_numeric_functions() {
    let (mut node, mut c) = funcs_node("prep").await;
    let stmt = c.prep("SELECT MOD(?, 3), SIGN(?)").await.expect("prep mod");
    let got: Vec<(i64, i64)> = c.exec(&stmt, (7i64, -5i64)).await.expect("exec mod");
    assert_eq!(got, vec![(1, -1)]);

    // Bound parameter as the whole predicate value.
    let stmt = c
        .prep("SELECT id FROM f WHERE price > ? ORDER BY id")
        .await
        .expect("prep price");
    let got: Vec<(i64,)> = c.exec(&stmt, (1.0f64,)).await.expect("exec price");
    assert_eq!(got, vec![(1,)]);
    node.kill_now();
}

#[tokio::test]
async fn numeric_negative_matrix() {
    let (mut node, mut c) = funcs_node("neg").await;

    // String operands into numeric-only functions are loud 1235s.
    for sql in [
        "SELECT ROUND('abc')",
        "SELECT SIGN('x')",
        "SELECT CEIL('abc')",
        "SELECT TRUNCATE('abc', 1)",
        "SELECT SQRT('abc')",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
    }
    // ROUND names its own domain in the message.
    let e = server_error(&mut c, "SELECT ROUND('abc')").await;
    assert!(e.message.contains("round"), "{}", e.message);

    // Mixed string/numeric extremes reject (no implicit coercion).
    let e = server_error(&mut c, "SELECT GREATEST('a', 1)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    let e = server_error(&mut c, "SELECT LEAST('a', 1)").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);

    // Integer overflow at the i64 rails is a loud server error with
    // MySQL's 1690 wording, never a silent wrap (P0: BIGINT out of
    // range). MIN is spelled `-9223372036854775807 - 1` because the
    // bare literal parses as a DOUBLE (i64::MAX + 1 magnitude).
    for sql in [
        "SELECT 9223372036854775807 + 1",
        "SELECT -9223372036854775807 - 2",
        "SELECT 4611686018427387904 * 2",
        "SELECT (-9223372036854775807 - 1) / -1",
        "SELECT -(-9223372036854775807 - 1)",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE, "{sql}: {}", e.message);
        assert!(
            e.message.contains("BIGINT value is out of range"),
            "{sql}: {}",
            e.message
        );
    }

    // Bad arity fails at prepare (1582).
    for sql in ["SELECT MOD(1)", "SELECT GREATEST()", "SELECT POW(2)"] {
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
    node.kill_now();
}
