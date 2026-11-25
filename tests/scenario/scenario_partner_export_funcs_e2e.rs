//! Multi-store scenario #137 -- a partner report is computed entirely
//! with SQL scalar functions and exported to S3: the function matrix
//! over literal inputs (numeric 12, string 20, the u64 bit operators,
//! the number->text coercions; every cell exact, NULL propagation
//! included), the per-partner projection over `partner_rows` (bitmask,
//! raised-flag, exact ROUND of score * 1.21, note), a CSV built from
//! the SQL cells, PUT/GET/HEAD on S3 (byte-exact body, Content-Length,
//! md5 ETag) and a RESP hash mirror of the gross column. One REAL
//! node, MySQL + S3 + RESP.
//! Deviation: PI/EXP/LN/LOG/CONV/CRC32/RAND/MID/SUBSTRING_INDEX/LTRIM/
//! RTRIM/ASCII/CHAR/FORMAT/STRCMP/SPACE/ELT/FIELD/QUOTE/MD5/SHA1/SHA2 and
//! unary `~` are unknown functions here (1235; src/sql/parse/expr.rs:75-77).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run, server_error, ER_NOT_SUPPORTED_YET};
use md5::Digest;
use mysql_async::Value as MVal;
use scenario_common::s3_extra::header_value;
use scenario_common::{front_world, int_of, r, rb, s3_call, FrontSpec};

const KEY: &str = "/rdb/exports/partner-fmt137.csv";

/// One wire cell as text (`<NULL>` for the NULL variant).
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

fn cells(row: &[MVal]) -> Vec<String> {
    row.iter().map(cell).collect()
}

/// (partner, flags, score in cents, note) of the fixture, Rust side.
const PARTNERS: [(&str, i64, i64, Option<&str>); 4] = [
    ("acme", 5, 1250, Some("preferred")),
    ("boris", 2, 333, Some("trial")),
    ("cedar", 15, 10000, Some("partner")),
    ("dover", 8, 5, None),
];

/// Cents spelled as the DECIMAL(10,2) literal the INSERT carries.
fn money(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Exact `ROUND(score * 1.21, 2)` for a two-decimal score: the product
/// is exact at scale 4 (cents * 121 is 1e-4 units / 100 = cents with a
/// two-digit fraction) and the round is half away from zero.
fn gross(cents: i64) -> String {
    let prod = cents * 121;
    let (neg, p) = (prod < 0, prod.abs());
    let (q, rem) = (p / 100, p % 100);
    let mut r = if rem * 2 >= 100 { q + 1 } else { q };
    if neg {
        r = -r;
    }
    format!("{}{}.{:02}", if r < 0 { "-" } else { "" }, r.abs() / 100, r.abs() % 100)
}

/// One CSV record: comma-joined cells, newline-terminated.
fn csv_line(cells: &[String]) -> String {
    format!("{}\n", cells.join(","))
}

/// The whole export document (header + one line per partner).
fn csv_document(data: &[Vec<String>]) -> String {
    let mut doc = csv_line(&[
        "partner".to_string(),
        "mask".to_string(),
        "raised".to_string(),
        "gross".to_string(),
        "note".to_string(),
        "tag".to_string(),
    ]);
    for row in data {
        doc.push_str(&csv_line(row));
    }
    doc
}

/// The expected per-partner projection, computed Rust side.
fn expected_partner_rows() -> Vec<Vec<String>> {
    PARTNERS
        .iter()
        .map(|(p, flags, cents, note)| {
            vec![
                (*p).to_string(),
                (flags & 3).to_string(),
                (flags | 8).to_string(),
                gross(*cents),
                note.unwrap_or("n/a").to_uppercase(),
                format!("{p}={flags}"),
            ]
        })
        .collect()
}

#[tokio::test]
async fn function_breadth_feeds_the_s3_export() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, a) = front_world("fmt137", &spec).await;
    let mut c = connect_root(&front.node).await;
    // Chinese + emoji literal, spelled with escapes (ASCII source).
    let zh = "\u{5361}\u{7247}\u{1F680}".to_string();

    // a) Hot block: one SELECT per function family over literal inputs,
    //    every cell exact. Numeric: rounding set, MOD/POW/SQRT/SIGN and
    //    the GREATEST/LEAST reducers (typed at the widest input).
    let t = scenario_common::perf::now();
    let got = cells(&rows(
        &mut c,
        "SELECT ABS(-7), CEIL(2.1), CEILING(-1.23), FLOOR(2.9), ROUND(2.567, 2), \
         ROUND(2.5), ROUND(-2.5), TRUNCATE(2.567, 2), TRUNCATE(122, -2), MOD(7, 3), \
         MOD(-7, 3), POW(2, 10), POWER(2, 10), SQRT(9), SQRT(-9), SIGN(-42), SIGN(0), \
         SIGN(2.5), GREATEST(2, 10, 3), LEAST(4, 2, 8), GREATEST(5, 2.10), LEAST(5, 2.5), \
         GREATEST(2, NULL, 3), ABS(NULL)",
    )
    .await
    .remove(0));
    assert_eq!(
        got,
        vec![
            "7", "3", "-1", "2", "2.57", "3", "-3", "2.56", "100", "1", "-1", "1024", "1024",
            "3", "<NULL>", "-1", "0", "1", "10", "2", "5.00", "2.5", "<NULL>", "<NULL>",
        ],
        "numeric family"
    );
    //    String: case, LENGTH vs CHAR_LENGTH on the UTF-8 literal, the
    //    CONCAT pair (NULL propagates vs NULL skipped), slicing (every
    //    SUBSTRING spelling), padding, search, REPLACE, the TRIM forms,
    //    HEX/UNHEX, and the number->text coercions (CONCAT('x=', 42),
    //    LPAD(7, 3, '0')).
    let got = cells(&rows(
        &mut c,
        &format!(
            "SELECT UPPER('hej'), LOWER('HEJ'), LENGTH('{zh}'), CHAR_LENGTH('{zh}'), \
             CONCAT('x=', 42), CONCAT('a', NULL), CONCAT_WS('-', 'a', NULL, 'c'), \
             CONCAT_WS(NULL, 'a', 'b'), SUBSTRING('Quadratically', 5, 6), \
             SUBSTR('Quadratically', -5), SUBSTRING('Quadratically' FROM 5 FOR 6), \
             LEFT('foobarbar', 4), RIGHT('foobarbar', 4), LPAD(7, 3, '0'), RPAD('hi', 5, '?'), \
             LPAD('hello', 2, '?'), LOCATE('bar', 'foobarbar', 5), INSTR('foobarbar', 'bar'), \
             POSITION('bar' IN 'foobarbar'), REPLACE('www.mysql.com', 'mysql', 'rdb'), \
             TRIM('  bar  '), TRIM(LEADING 'x' FROM 'xxxbarxxx'), \
             TRIM(TRAILING 'x' FROM 'xxxbarxxx'), REVERSE('abc'), REVERSE(123), \
             REPEAT('ab', 3), HEX('abc'), HEX(255), UNHEX('4D7953514C'), UNHEX('zz'), \
             LEFT('abcdef', NULL), UPPER(NULL)"
        ),
    )
    .await
    .remove(0));
    let mut want: Vec<String> = ["HEJ", "hej"].iter().map(|s| s.to_string()).collect();
    want.push(zh.len().to_string());
    want.push(zh.chars().count().to_string());
    want.extend(
        [
            "x=42", "<NULL>", "a-c", "<NULL>", "ratica", "cally", "ratica", "foob", "rbar", "007",
            "hi???", "he", "7", "4", "4", "www.rdb.com", "bar", "barxxx", "xxxbar", "cba", "321",
            "ababab", "616263", "00000000000000FF", "MySQL", "<NULL>", "<NULL>", "<NULL>",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    assert_eq!(got, want, "string family");
    //    Bitwise: MySQL's u64 semantics; NULL propagates.
    let got = cells(&rows(
        &mut c,
        "SELECT 5 & 3, 5 | 3, 5 ^ 3, 1 << 10, 1024 >> 3, -1 & 255, 1 << 63, 1 << 64, NULL & 1",
    )
    .await
    .remove(0));
    assert_eq!(
        got,
        vec![
            "1", "7", "6", "1024", "128", "255", "-9223372036854775808", "0", "<NULL>",
        ],
        "bitwise operators"
    );
    //    Not implemented on this server (recorded, not asserted as
    //    MySQL would answer): the function-less names are unknown
    //    functions (1235) and the unary `~` is an unsupported unary.
    for sql in [
        "SELECT PI()",
        "SELECT EXP(1)",
        "SELECT LN(1)",
        "SELECT CONV('a', 16, 2)",
        "SELECT CRC32('a')",
        "SELECT RAND()",
        "SELECT MID('Quadratically', 5, 6)",
        "SELECT SUBSTRING_INDEX('a,b,c', ',', 2)",
        "SELECT LTRIM('  x')",
        "SELECT RTRIM('x  ')",
        "SELECT ASCII('a')",
        "SELECT FORMAT(1234.567, 2)",
        "SELECT STRCMP('a', 'b')",
        "SELECT SPACE(3)",
        "SELECT ELT(2, 'a', 'b')",
        "SELECT FIELD('b', 'a', 'b')",
        "SELECT QUOTE('a')",
        "SELECT MD5('a')",
        "SELECT SHA1('a')",
        "SELECT SHA2('a', 256)",
        "SELECT ~5",
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
        assert!(
            e.message.contains("unknown function") || e.message.contains("unary"),
            "{sql}: {e:?}"
        );
    }
    scenario_common::perf::within("fmt137/function-matrix", t, Duration::from_secs(2));

    // b) The partner table: 4 rows, one NULL note.
    ddl(
        &mut c,
        "CREATE TABLE partner_rows (partner VARCHAR(16) PRIMARY KEY, flags INT, \
         score DECIMAL(10,2), note VARCHAR(64))",
    )
    .await;
    let values: Vec<String> = PARTNERS
        .iter()
        .map(|(p, flags, cents, note)| {
            let n = match note {
                Some(s) => format!("'{s}'"),
                None => "NULL".to_string(),
            };
            format!("('{p}', {flags}, {}, {n})", money(*cents))
        })
        .collect();
    run(
        &mut c,
        &format!(
            "INSERT INTO partner_rows (partner, flags, score, note) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
    let report = rows(
        &mut c,
        "SELECT partner, flags & 3, flags | 8, ROUND(score * 1.21, 2), \
         UPPER(COALESCE(note, 'n/a')), CONCAT(partner, '=', flags) \
         FROM partner_rows ORDER BY partner",
    )
    .await;
    let want = expected_partner_rows();
    let report: Vec<Vec<String>> = report.iter().map(|r| cells(r)).collect();
    assert_eq!(report, want, "partner projection (Rust-side expectation)");

    // c) The CSV document is built from the SQL cells; S3 PUT/GET/HEAD
    //    must agree with it byte for byte (ETag = quoted md5 hex).
    let doc = csv_document(&report);
    let body = doc.as_bytes().to_vec();
    let mut hasher = md5::Md5::new();
    md5::Digest::update(&mut hasher, &body);
    let md5hex = hex::encode(md5::Digest::finalize(hasher));
    let t = scenario_common::perf::now();
    let (status, head, _) = s3_call(&front.s3, "PUT", KEY, &body).await;
    assert_eq!(status, 200, "put: {head}");
    let etag = header_value(&head, "ETag").expect("PUT ETag").to_string();
    assert_eq!(etag, format!("\"{md5hex}\""), "ETag is the body md5");
    let (status, head, got) = s3_call(&front.s3, "GET", KEY, b"").await;
    assert_eq!(status, 200, "get: {head}");
    assert_eq!(got, body, "GET body == exported CSV");
    assert_eq!(header_value(&head, "Content-Type"), Some("application/octet-stream"));
    let (status, head, got) = s3_call(&front.s3, "HEAD", KEY, b"").await;
    assert_eq!(status, 200, "head: {head}");
    assert!(got.is_empty(), "HEAD carries headers only");
    let length = body.len().to_string();
    assert_eq!(header_value(&head, "Content-Length"), Some(length.as_str()));
    assert_eq!(header_value(&head, "ETag"), Some(etag.as_str()));
    scenario_common::perf::within("fmt137/s3-export", t, Duration::from_secs(2));

    // d) Cross-checks: SQL COUNT(*) == CSV data lines == RESP mirror
    //    (one HSET per SQL cell), and the gross column reads back.
    let count = cells(&rows(&mut c, "SELECT COUNT(*) FROM partner_rows").await.remove(0))[0]
        .clone();
    let lines = doc.matches('\n').count();
    assert_eq!(count, "4", "SQL row count");
    assert_eq!(lines, PARTNERS.len() + 1, "header + one CSV line per row");
    for (p, _, cents, _) in PARTNERS {
        let (k, f, v) = (
            "fmt137:gross".to_string(),
            p.to_string(),
            gross(cents),
        );
        assert_eq!(
            int_of(&r(&a, &[b"HSET", k.as_bytes(), f.as_bytes(), v.as_bytes()]).await),
            1,
            "hset mirror field"
        );
    }
    assert_eq!(int_of(&r(&a, &[b"HLEN", b"fmt137:gross"]).await), 4, "mirror size");
    assert_eq!(rb(&a, &[b"HGET", b"fmt137:gross", b"acme"]).await, report[0][3], "mirror acme");
    assert_eq!(rb(&a, &[b"HGET", b"fmt137:gross", b"dover"]).await, report[3][3], "mirror dover");
    scenario_common::perf::scenario("fmt137", t0);
}
