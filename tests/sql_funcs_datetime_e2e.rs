//! M1 datetime-family function e2e over a real rdb MySQL-protocol process
//! (`plans/2026-10-06-mysql-gap/m1-expression-functions.md`; scaffolding
//! in `tests/common/mysql.rs`): clock-family shapes + metadata, DATE
//! extraction (invalid -> NULL), DATE_ADD/SUB across units incl. the
//! `d +/- INTERVAL n unit` operator form and month/leap clamping,
//! DATEDIFF, DATE_FORMAT specifiers, the UNIX_TIMESTAMP pair, functions
//! as WHERE / GROUP BY keys, and the loud INTERVAL-unit rejections.

mod common;

use common::mysql::{
    assert_exprs, col, col_types, ddl, funcs_node, i, one, rows, s, server_error,
    ER_NOT_SUPPORTED_YET, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
};
use mysql_async::consts::ColumnType::*;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

/// `YYYY-MM-DD` shape of a text cell.
fn looks_like_date(v: &MVal) -> bool {
    let MVal::Bytes(b) = v else {
        return false;
    };
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit)
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` shape (the clock runs at full
/// microsecond precision, so NOW() carries the 6-digit fraction).
fn looks_like_datetime(v: &MVal) -> bool {
    let MVal::Bytes(b) = v else {
        return false;
    };
    let frac_ok = match b.len() {
        19 => true,
        26 => b[19] == b'.' && b[20..].iter().all(u8::is_ascii_digit),
        _ => return false,
    };
    looks_like_date(&MVal::Bytes(b[..10].to_vec()))
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[17..19].iter().all(u8::is_ascii_digit)
        && frac_ok
}

#[tokio::test]
async fn clock_family_shapes_and_metadata() {
    let (mut node, mut c) = funcs_node("clock").await;
    for sql in ["NOW()", "SYSDATE()", "CURRENT_TIMESTAMP", "LOCALTIMESTAMP"] {
        let got = one(&mut c, &format!("SELECT {sql}")).await;
        assert!(looks_like_datetime(&got), "{sql}: {got:?}");
    }
    for sql in ["CURDATE()", "CURRENT_DATE"] {
        let got = one(&mut c, &format!("SELECT {sql}")).await;
        assert!(looks_like_date(&got), "{sql}: {got:?}");
    }
    // CURTIME renders HH:MM:SS text (00..23 hours).
    let got = one(&mut c, "SELECT CURTIME()").await;
    let MVal::Bytes(b) = &got else {
        panic!("curtime: {got:?}")
    };
    let t = std::str::from_utf8(b).unwrap();
    assert_eq!(t.len(), 8, "{t}");
    assert_eq!(&t[2..3], ":");
    assert!(&t[0..2] <= "23", "{t}");

    // Result metadata: DATETIME / DATE clocks, text time.
    assert_eq!(
        col_types(&mut c, "SELECT NOW(), CURDATE(), CURTIME(), SYSDATE()").await,
        vec![
            MYSQL_TYPE_DATETIME,
            MYSQL_TYPE_DATE,
            MYSQL_TYPE_VAR_STRING,
            MYSQL_TYPE_DATETIME
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn extraction_functions() {
    let (mut node, mut c) = funcs_node("extract").await;
    assert_exprs(
        &mut c,
        &[
            ("YEAR('2024-03-05 06:07:08')", i(2024)),
            ("MONTH('2024-03-05 06:07:08')", i(3)),
            ("DAY('2024-03-05 06:07:08')", i(5)),
            ("DAYOFMONTH('2024-03-05 06:07:08')", i(5)),
            ("HOUR('2024-03-05 06:07:08')", i(6)),
            ("MINUTE('2024-03-05 06:07:08')", i(7)),
            ("SECOND('2024-03-05 06:07:08')", i(8)),
            // A bare date is midnight.
            ("HOUR('2024-01-02')", i(0)),
            ("DATE('2024-01-02 03:04:05')", s("2024-01-02")),
            ("DATE('20240102')", s("2024-01-02")),
            // Invalid spellings and NULL are NULL, not errors.
            ("YEAR('junk')", MVal::NULL),
            ("SECOND('nope')", MVal::NULL),
            ("YEAR(NULL)", MVal::NULL),
            ("DATE('not-a-date')", MVal::NULL),
        ],
    )
    .await;
    // Extraction off real columns: YEAR of the id-2 DATETIME, MONTH of
    // the id-4 DATE.
    let got = one(&mut c, "SELECT YEAR(ts) FROM f WHERE id = 2").await;
    assert_eq!(got, i(2024));
    let got = one(&mut c, "SELECT MONTH(d) FROM f WHERE id = 4").await;
    assert_eq!(got, i(3));
    // Extraction answers BIGINT, DATE() answers a DATE column.
    assert_eq!(
        col_types(&mut c, "SELECT YEAR(ts), MONTH(d), DATE(ts) FROM f").await,
        vec![MYSQL_TYPE_LONGLONG, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_DATE]
    );
    node.kill_now();
}

#[tokio::test]
async fn date_arithmetic_and_interval_forms() {
    let (mut node, mut c) = funcs_node("arith").await;
    assert_exprs(
        &mut c,
        &[
            // Function + INTERVAL forms.
            ("DATE_ADD('2024-01-02', INTERVAL 3 DAY)", s("2024-01-05")),
            ("DATE_SUB('2024-01-02', INTERVAL 1 DAY)", s("2024-01-01")),
            // Month/leap clamping.
            ("DATE_ADD('2024-01-31', INTERVAL 1 MONTH)", s("2024-02-29")),
            ("DATE_ADD('2024-02-29', INTERVAL 1 YEAR)", s("2025-02-28")),
            // Time units promote a bare date to a midnight DATETIME.
            (
                "DATE_ADD('2024-01-02', INTERVAL 25 HOUR)",
                s("2024-01-03 01:00:00"),
            ),
            (
                "DATE_ADD('2024-01-02 03:04:05', INTERVAL 90 MINUTE)",
                s("2024-01-02 04:34:05"),
            ),
            (
                "DATE_ADD('2024-01-02 03:04:05', INTERVAL -10 SECOND)",
                s("2024-01-02 03:03:55"),
            ),
            (
                "DATE_ADD('2024-01-02 03:04:05', INTERVAL 1000000 MICROSECOND)",
                s("2024-01-02 03:04:06"),
            ),
            // The `d +/- INTERVAL n unit` operator spellings.
            ("'2024-01-02' + INTERVAL 3 DAY", s("2024-01-05")),
            ("'2024-01-02' - INTERVAL 1 DAY", s("2024-01-01")),
            ("INTERVAL 3 DAY + '2024-01-02'", s("2024-01-05")),
            // ADDDATE/SUBDATE plain-days spellings.
            ("ADDDATE('2024-01-02', 3)", s("2024-01-05")),
            ("SUBDATE('2024-01-02', 1)", s("2024-01-01")),
            // Bad inputs stay NULL (permissive read path).
            ("DATE_ADD('junk', INTERVAL 1 DAY)", MVal::NULL),
            ("DATE_ADD(NULL, INTERVAL 1 DAY)", MVal::NULL),
        ],
    )
    .await;
    // Column form: a DATE stays a DATE for date units.
    let got = one(&mut c, "SELECT d + INTERVAL 3 DAY FROM f WHERE id = 1").await;
    assert_eq!(got, s("2024-01-05"));
    let got = one(
        &mut c,
        "SELECT DATE_SUB(ts, INTERVAL 90 MINUTE) FROM f WHERE id = 1",
    )
    .await;
    assert_eq!(got, s("2024-01-02 01:34:05"));
    assert_eq!(
        col_types(
            &mut c,
            "SELECT d + INTERVAL 1 DAY, ts - INTERVAL 1 MINUTE FROM f"
        )
        .await,
        vec![MYSQL_TYPE_DATE, MYSQL_TYPE_DATETIME]
    );
    node.kill_now();
}

#[tokio::test]
async fn datediff_and_unix_epoch_pair() {
    let (mut node, mut c) = funcs_node("epoch").await;
    assert_exprs(
        &mut c,
        &[
            ("DATEDIFF('2024-01-02', '2024-01-01')", i(1)),
            ("DATEDIFF('2024-01-01', '2024-01-02')", i(-1)),
            // Date parts only: the time-of-day never counts.
            (
                "DATEDIFF('2024-01-02 23:00:00', '2024-01-01 00:00:00')",
                i(1),
            ),
            ("DATEDIFF(NULL, '2024-01-01')", MVal::NULL),
            ("DATEDIFF('junk', '2024-01-01')", MVal::NULL),
            ("UNIX_TIMESTAMP('2024-01-02 03:04:05')", i(1704164645)),
            ("UNIX_TIMESTAMP('2024-01-02')", i(1704153600)),
            ("UNIX_TIMESTAMP('junk')", MVal::NULL),
            ("FROM_UNIXTIME(0)", s("1970-01-01 00:00:00")),
            ("FROM_UNIXTIME(1704164645)", s("2024-01-02 03:04:05")),
            // Out-of-range seconds and NULL are NULL (MySQL), not errors.
            ("FROM_UNIXTIME(922337203685477580)", MVal::NULL),
            ("FROM_UNIXTIME(NULL)", MVal::NULL),
        ],
    )
    .await;
    // No-arg UNIX_TIMESTAMP rides the same UTC clock as NOW(); allow a
    // few seconds of slack for the round trip.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let got = one(&mut c, "SELECT UNIX_TIMESTAMP()").await;
    let MVal::Bytes(b) = &got else {
        panic!("unix_timestamp(): {got:?}")
    };
    let v: i64 = std::str::from_utf8(b).unwrap().parse().unwrap();
    assert!((v - now_secs).abs() <= 5, "{v} vs {now_secs}");

    // DATEDIFF answers BIGINT; FROM_UNIXTIME renders text (v1 keeps
    // the string shape, not a DATETIME column).
    assert_eq!(
        col_types(
            &mut c,
            "SELECT DATEDIFF(d, '2024-01-01'), FROM_UNIXTIME(0) FROM f"
        )
        .await,
        vec![MYSQL_TYPE_LONGLONG, MYSQL_TYPE_VAR_STRING]
    );
    node.kill_now();
}

#[tokio::test]
async fn date_format_specifiers() {
    let (mut node, mut c) = funcs_node("fmt").await;
    assert_exprs(
        &mut c,
        &[
            (
                "DATE_FORMAT('2024-02-29 13:05:09', '%Y-%m-%d')",
                s("2024-02-29"),
            ),
            ("DATE_FORMAT('2024-02-29 13:05:09', '%y')", s("24")),
            (
                "DATE_FORMAT('2024-02-29 13:05:09', '%H:%i:%s')",
                s("13:05:09"),
            ),
            ("DATE_FORMAT('2024-02-29 13:05:09', '%T')", s("13:05:09")),
            ("DATE_FORMAT('2024-02-29 13:05:09', '%p')", s("PM")),
            ("DATE_FORMAT('1970-01-01', '%W')", s("Thursday")),
            ("DATE_FORMAT('2024-02-29 13:05:09', '%M')", s("February")),
            ("DATE_FORMAT('2024-02-29 13:05:09', '%j')", s("060")),
            ("DATE_FORMAT('2024-02-29 13:05:09', '%%')", s("%")),
            // A bare date is midnight for the time specs.
            ("DATE_FORMAT('1970-01-01', '%H %i %s %p')", s("00 00 00 AM")),
            ("DATE_FORMAT(NULL, '%Y')", MVal::NULL),
            ("DATE_FORMAT('junk', '%Y')", MVal::NULL),
        ],
    )
    .await;
    node.kill_now();
}

#[tokio::test]
async fn datetime_functions_in_where_and_group_by() {
    let (mut node, mut c) = funcs_node("keys").await;
    ddl(
        &mut c,
        "CREATE TABLE ev (id BIGINT PRIMARY KEY, at DATETIME)",
    )
    .await;
    c.query_drop(
        "INSERT INTO ev (id, at) VALUES \
         (1, '2024-03-05 08:00:00'), (2, '2024-03-05 19:30:00'), (3, '2024-03-06 07:00:00')",
    )
    .await
    .expect("seed ev");

    // WHERE over an extraction.
    let got = col(&mut c, "SELECT id FROM ev WHERE HOUR(at) = 8").await;
    assert_eq!(got, vec![i(1)]);
    let got = col(
        &mut c,
        "SELECT id FROM f WHERE YEAR(ts) = 2024 AND MONTH(d) = 3 ORDER BY id",
    )
    .await;
    assert_eq!(got, vec![i(4)]);

    // GROUP BY over a computed date key: two rows share 2024-03-05.
    let got = rows(
        &mut c,
        "SELECT DATE(at) AS day, COUNT(*) AS n FROM ev GROUP BY DATE(at) ORDER BY day",
    )
    .await;
    assert_eq!(
        got,
        vec![vec![s("2024-03-05"), i(2)], vec![s("2024-03-06"), i(1)]]
    );
    node.kill_now();
}

#[tokio::test]
async fn datetime_negative_matrix() {
    let (mut node, mut c) = funcs_node("neg").await;

    // INTERVAL units beyond the supported set reject loudly.
    for sql in [
        "SELECT DATE_ADD('2024-01-02', INTERVAL 1 WEEK)",
        "SELECT DATE_ADD('2024-01-02', INTERVAL 1 QUARTER)",
        "SELECT '2024-01-02' + INTERVAL 1 WEEK",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {}", e.message);
        assert!(
            e.message.to_uppercase().contains("INTERVAL UNIT"),
            "{sql}: {}",
            e.message
        );
        assert!(
            e.message.to_uppercase().contains("YEAR/MONTH/DAY"),
            "{sql}: {}",
            e.message
        );
    }
    // DATE_ADD without INTERVAL is the wrong parameter count (1582).
    let e = server_error(&mut c, "SELECT DATE_ADD('2024-01-02', 1)").await;
    assert_eq!(e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT, "{}", e.message);
    // Bad arity fails at prepare.
    for sql in [
        "SELECT YEAR()",
        "SELECT DATEDIFF('2024-01-01')",
        "SELECT DATE_FORMAT('2024-01-01')",
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
    // An unparseable *date value* feeding a function is NULL (the
    // permissive read path), not an error; the 1292 write-path
    // rejection of bad column literals is owned by sql_types_e2e.
    let got = one(&mut c, "SELECT YEAR('9999-99-99')").await;
    assert_eq!(got, MVal::NULL);
    node.kill_now();
}
