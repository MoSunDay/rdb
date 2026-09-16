//! Phase-2 temporal-type e2e over one real rdb process: DATE /
//! DATETIME / TIMESTAMP DDL, SHOW COLUMNS types, text roundtrips
//! (canonical/fractional/compact literals, leap day), predicates,
//! NOW()/CURDATE() smoke, typed result-column metadata, prepared
//! binary cells and temporal params, unique date index, UNION widening.

mod common;

use common::{
    mysql_root_conn, mysql_server_error, spawn_node_mysql, wait_mysql_ready, wait_resp_ready,
};
use mysql_async::consts::{ColumnType, ColumnType::*};
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

const PASS: &str = "e2e-sql-pass";

/// DDL needs the raft leader; retry until the bootstrap node becomes one.
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

/// Sorted text cells of a single-column result (order unspecified).
async fn sorted_cells(conn: &mut mysql_async::Conn, sql: &str) -> Vec<MVal> {
    let mut cells: Vec<MVal> = rows(conn, sql)
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect();
    cells.sort_by_key(|c| format!("{c:?}"));
    cells
}

fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}

/// First row's column wire types (metadata assertions).
async fn col_types(conn: &mut mysql_async::Conn, sql: &str) -> Vec<ColumnType> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    let row = rs.first().unwrap_or_else(|| panic!("no rows for {sql}"));
    row.columns_ref().iter().map(|c| c.column_type()).collect()
}

fn digits(b: &[u8]) -> bool {
    !b.is_empty() && b.iter().all(u8::is_ascii_digit)
}

/// `YYYY-MM-DD` shape of a text cell.
fn looks_like_date(v: &MVal) -> bool {
    let MVal::Bytes(b) = v else { return false };
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && digits(&b[..4])
        && digits(&b[5..7])
        && digits(&b[8..10])
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` shape (fraction, when present, is
/// the full six digits) with a plausible date head.
fn looks_like_datetime(v: &MVal) -> bool {
    let MVal::Bytes(b) = v else { return false };
    let frac_ok = match b.len() {
        19 => true,
        26 => b[19] == b'.' && digits(&b[20..]),
        _ => return false,
    };
    looks_like_date(&MVal::Bytes(b[..10].to_vec()))
        && b[10] == b' '
        && b[13] == b':'
        && b[16] == b':'
        && digits(&b[11..13])
        && digits(&b[14..16])
        && digits(&b[17..19])
        && frac_ok
}

/// A plausible civil value: shape plus year 2000..=9999.
fn plausible(v: &MVal, with_time: bool) -> bool {
    let shaped = if with_time {
        looks_like_datetime(v)
    } else {
        looks_like_date(v)
    };
    let MVal::Bytes(b) = v else { return false };
    if !shaped {
        return false;
    }
    let y: u32 = std::str::from_utf8(&b[..4]).unwrap().parse().unwrap();
    (2000..=9999).contains(&y)
}

async fn world(name: &str, seed: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-types-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut conn = mysql_root_conn(&node, PASS).await;
    ddl(
        &mut conn,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, d DATE, dt DATETIME, ts TIMESTAMP, note VARCHAR(50))",
    )
    .await;
    conn.query_drop(seed).await.expect("seed t");
    (node, conn)
}

#[tokio::test]
async fn temporal_ddl_text_roundtrip_and_predicates() {
    let (mut node, mut c) = world(
        "text",
        "INSERT INTO t (id, d, dt, ts, note) VALUES \
         (1, '2024-02-29', '2024-02-29 01:02:03', '2024-02-29 01:02:03', 'leap'), \
         (2, '2024-03-01', '2024-02-29 01:02:03.123456', '2024-02-29 01:02:03.123456', 'frac'), \
         (3, '20240229', '20240229010203', '20240229010203', 'compact')",
    )
    .await;

    // SHOW COLUMNS reports the temporal type strings (TIMESTAMP is a
    // plain DATETIME alias).
    let mut cols = rows(&mut c, "SHOW COLUMNS FROM t").await;
    let mut shown = Vec::new();
    for r in cols.iter_mut() {
        shown.append(&mut r[..2].to_vec());
    }
    assert_eq!(
        shown,
        vec![
            s("id"),
            s("bigint"),
            s("d"),
            s("date"),
            s("dt"),
            s("datetime"),
            s("ts"),
            s("datetime"),
            s("note"),
            s("varchar"),
        ]
    );

    // Text roundtrip: canonical spelling out, compact literals
    // normalized, fraction preserved exactly (leap day included).
    let got = rows(&mut c, "SELECT d, dt, ts FROM t WHERE id = 1").await;
    assert_eq!(
        got,
        vec![vec![
            s("2024-02-29"),
            s("2024-02-29 01:02:03"),
            s("2024-02-29 01:02:03")
        ]]
    );
    let got = rows(&mut c, "SELECT d, dt, ts FROM t WHERE id = 2").await;
    assert_eq!(
        got,
        vec![vec![
            s("2024-03-01"),
            s("2024-02-29 01:02:03.123456"),
            s("2024-02-29 01:02:03.123456")
        ]]
    );
    let got = rows(&mut c, "SELECT d, dt FROM t WHERE id = 3").await;
    assert_eq!(
        got,
        vec![vec![s("2024-02-29"), s("2024-02-29 01:02:03")]],
        "compact literals normalize to canonical"
    );

    // Garbage literals fail the whole INSERT loudly (MySQL "Incorrect
    // DATE value"; errno is MySQL's own 1292 (ER_TRUNCATED_WRONG_VALUE).
    let e = mysql_server_error(&mut c, "INSERT INTO t (id, d) VALUES (4, '2024-02-30')").await;
    assert_eq!(e.code, 1292);
    assert!(e.message.contains("Incorrect DATE value"), "{}", e.message);
    assert!(c
        .query_drop("INSERT INTO t (id, dt) VALUES (4, '2024-02-29 25:00:00')")
        .await
        .is_err());
    let got = rows(&mut c, "SELECT COUNT(*) FROM t").await;
    assert_eq!(got, vec![vec![s("3")]], "rejected rows never landed");

    // ORDER BY over days.
    let got = rows(&mut c, "SELECT id FROM t ORDER BY d DESC, id DESC").await;
    assert_eq!(got, vec![vec![s("2")], vec![s("3")], vec![s("1")]]);

    // Predicates: string literals compare in the temporal domain.
    let got = sorted_cells(&mut c, "SELECT id FROM t WHERE d = '2024-02-29'").await;
    assert_eq!(got, vec![s("1"), s("3")]);
    assert_eq!(
        rows(&mut c, "SELECT id FROM t WHERE d >= '2024-03-01'").await,
        vec![vec![s("2")]]
    );
    let got = sorted_cells(
        &mut c,
        "SELECT id FROM t WHERE dt BETWEEN '2024-02-29 00:00:00' AND '2024-03-01'",
    )
    .await;
    assert_eq!(got, vec![s("1"), s("2"), s("3")]);

    // UPDATE writes through the same coercion.
    c.query_drop("UPDATE t SET d = '2025-01-01' WHERE id = 1")
        .await
        .expect("update");
    assert_eq!(
        rows(&mut c, "SELECT d FROM t WHERE id = 1").await,
        vec![vec![s("2025-01-01")]]
    );

    // NOW()/CURRENT_TIMESTAMP/CURDATE() smoke: plausible spellings.
    let now = rows(&mut c, "SELECT NOW()").await;
    assert!(plausible(&now[0][0], true));
    let cur = rows(&mut c, "SELECT CURRENT_TIMESTAMP()").await;
    assert!(plausible(&cur[0][0], true));
    let day = rows(&mut c, "SELECT CURDATE()").await;
    assert!(plausible(&day[0][0], false));

    // Aggregates over temporal columns are NULL (no numeric meaning).
    let got = rows(&mut c, "SELECT SUM(d), AVG(d) FROM t").await;
    assert_eq!(got, vec![vec![MVal::NULL, MVal::NULL]]);
    node.kill_now();
}

#[tokio::test]
async fn result_metadata_is_typed() {
    let (mut node, mut c) = world("meta", "INSERT INTO t (id, d) VALUES (1, '2024-02-29')").await;
    ddl(
        &mut c,
        "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(8))",
    )
    .await;
    c.query_drop("INSERT INTO ai (v) VALUES ('x')")
        .await
        .expect("auto insert");

    // LAST_INSERT_ID() is BIGINT-typed metadata (the old VARCHAR
    // deviation is gone).
    assert_eq!(
        col_types(&mut c, "SELECT LAST_INSERT_ID()").await,
        vec![MYSQL_TYPE_LONGLONG]
    );
    assert_eq!(
        col_types(&mut c, "SELECT 1").await,
        vec![MYSQL_TYPE_LONGLONG]
    );
    assert_eq!(
        col_types(&mut c, "SELECT LENGTH('abc')").await,
        vec![MYSQL_TYPE_LONGLONG]
    );
    assert_eq!(
        col_types(&mut c, "SELECT 1.5").await,
        vec![MYSQL_TYPE_NEWDECIMAL]
    );
    // Temporal functions and table columns announce their domains.
    assert_eq!(
        col_types(&mut c, "SELECT NOW()").await,
        vec![MYSQL_TYPE_DATETIME]
    );
    assert_eq!(
        col_types(&mut c, "SELECT CURDATE()").await,
        vec![MYSQL_TYPE_DATE]
    );
    assert_eq!(
        col_types(&mut c, "SELECT id, d, dt, ts, note FROM t").await,
        vec![
            MYSQL_TYPE_LONGLONG,
            MYSQL_TYPE_DATE,
            MYSQL_TYPE_DATETIME,
            MYSQL_TYPE_DATETIME,
            MYSQL_TYPE_VAR_STRING,
        ]
    );
    node.kill_now();
}

#[tokio::test]
async fn prepared_binary_cells_params_unique_index_union() {
    let (mut node, mut c) = world(
        "prep",
        "INSERT INTO t (id, d, dt, ts, note) VALUES \
         (1, '2024-02-29', '2024-02-29 01:02:03', '2024-02-29 01:02:03', 'leap'), \
         (2, '2024-03-01', '2024-02-29 01:02:03.123456', '2024-02-29 01:02:03.123456', 'frac'), \
         (3, '2024-03-02', '2024-03-01 00:00:00', '2024-03-01 00:00:00', 'plain')",
    )
    .await;

    // Binary protocol cells arrive as typed mysql Values (Date carries
    // datetimes too): fraction dropped when zero, exact when not.
    let rs: Vec<(MVal, MVal)> = c
        .exec("SELECT d, dt FROM t WHERE id = ?", (1i64,))
        .await
        .expect("prepared select 1");
    assert_eq!(
        rs,
        vec![(
            MVal::Date(2024, 2, 29, 0, 0, 0, 0),
            MVal::Date(2024, 2, 29, 1, 2, 3, 0)
        )]
    );
    let rs: Vec<(MVal, MVal)> = c
        .exec("SELECT d, dt FROM t WHERE id = ?", (2i64,))
        .await
        .expect("prepared select 2");
    assert_eq!(
        rs,
        vec![(
            MVal::Date(2024, 3, 1, 0, 0, 0, 0),
            MVal::Date(2024, 2, 29, 1, 2, 3, 123456)
        )]
    );

    // EXECUTE with a temporal predicate bound as string bytes (client
    // sends Bytes -> engine Str -> canonical parse).
    let rs: Vec<(MVal,)> = c
        .exec("SELECT note FROM t WHERE d = ?", ("2024-02-29",))
        .await
        .expect("string param");
    assert_eq!(rs, vec![(s("leap"),)]);

    // A typed client Date parameter (binary datetime form, midnight)
    // decodes and compares cross-scale against the DATE column.
    let rs: Vec<(MVal,)> = c
        .exec(
            "SELECT note FROM t WHERE d = ?",
            (MVal::Date(2024, 2, 29, 0, 0, 0, 0),),
        )
        .await
        .expect("typed date param");
    assert_eq!(rs, vec![(s("leap"),)]);

    // Prepared INSERT with temporal strings as parameters.
    c.exec_drop(
        "INSERT INTO t (id, d, dt, ts, note) VALUES (?, ?, ?, ?, ?)",
        (
            5i64,
            "2024-05-06",
            "2024-05-06 07:08:09",
            "2024-05-06 07:08:09",
            "param",
        ),
    )
    .await
    .expect("prepared insert");
    let got = rows(&mut c, "SELECT d, dt FROM t WHERE id = 5").await;
    assert_eq!(got, vec![vec![s("2024-05-06"), s("2024-05-06 07:08:09")]]);

    // Unique index over a date column: MySQL 1062 on duplicates.
    ddl(&mut c, "CREATE UNIQUE INDEX uq_d ON t (d)").await;
    let e = mysql_server_error(&mut c, "INSERT INTO t (id, d) VALUES (9, '2024-02-29')").await;
    assert_eq!(e.code, 1062);
    assert!(e.message.contains("Duplicate entry"), "{}", e.message);
    c.query_drop("INSERT INTO t (id, d) VALUES (8, '2024-03-05')")
        .await
        .expect("distinct date accepted");

    // UNION widens date -> datetime; the NULL dt of row 8 rides along.
    let got = sorted_cells(&mut c, "SELECT d FROM t UNION SELECT dt FROM t").await;
    assert_eq!(
        got,
        vec![
            s("2024-02-29 00:00:00"),
            s("2024-02-29 01:02:03"),
            s("2024-02-29 01:02:03.123456"),
            s("2024-03-01 00:00:00"),
            s("2024-03-02 00:00:00"),
            s("2024-03-05 00:00:00"),
            s("2024-05-06 00:00:00"),
            s("2024-05-06 07:08:09"),
            MVal::NULL,
        ]
    );
    node.kill_now();
}

/// Unsupported column types reject loudly at translate time (MySQL
/// 1235, the type named in the message), and the zero-date literal is
/// an incorrect value (MySQL 1292) instead of a silent epoch/NULL.
#[tokio::test]
async fn unsupported_and_zero_temporal_values_loud() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-types-loud-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // v1 keeps DATE/DATETIME/TIMESTAMP and drops the rest of the
    // temporal/numeric zoo by name (see translate.rs). DECIMAL gained
    // first-class support, so only TIME is loud here.
    for (sql, ty) in [("CREATE TABLE t (d TIME)", "TIME")] {
        let e = mysql_server_error(&mut c, sql).await;
        assert_eq!(e.code, 1235, "{sql}: {}", e.message);
        assert!(e.message.contains(ty), "{sql}: {}", e.message);
    }

    // '0000-00-00' is outside the DATE domain (year 0001..=9999, real
    // month/day): MySQL 1292, nothing stored.
    ddl(&mut c, "CREATE TABLE ok (id BIGINT PRIMARY KEY, d DATE)").await;
    let e = mysql_server_error(&mut c, "INSERT INTO ok (id, d) VALUES (1, '0000-00-00')").await;
    assert_eq!(e.code, 1292, "{}", e.message);
    assert!(e.message.contains("Incorrect DATE value"), "{}", e.message);
    let got = rows(&mut c, "SELECT COUNT(*) FROM ok").await;
    assert_eq!(got, vec![vec![s("0")]], "nothing stored");
    node.kill_now();
}

/// DECIMAL end to end: exact literal arithmetic (0.1 + 0.2 is 0.30,
/// never a binary-float 0.30000000000000004), half-away-from-zero
/// rounding on writes, SUM/AVG widening, exact comparisons through a
/// secondary index, DESCRIBE and the precision edge.
#[tokio::test]
async fn decimal_exact_semantics_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-types-dec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // Exact literals: plain int.frac text never touches an f64. The sum
    // renders at scale max(1,1)=1 like MySQL, and 0.1+0.2 is exactly 0.3.
    assert_eq!(rows(&mut c, "SELECT 0.1 + 0.2").await, vec![vec![s("0.3")]]);
    assert_eq!(rows(&mut c, "SELECT 1 / 3").await, vec![vec![s("0")]]);
    assert_eq!(
        rows(&mut c, "SELECT 1.0 / 3").await,
        vec![vec![s("0.33333")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT -0.05 / 0.10").await,
        vec![vec![s("-0.500000")]]
    );

    // Column writes round half away from zero at the declared scale.
    ddl(
        &mut c,
        "CREATE TABLE dec (id BIGINT PRIMARY KEY, v DECIMAL(10,2))",
    )
    .await;
    for (id, v) in [(1, "1.005"), (2, "2.344"), (3, "-1.005"), (4, "NULL")] {
        c.query_drop(format!("INSERT INTO dec (id, v) VALUES ({id}, {v})"))
            .await
            .expect("seed dec");
    }
    assert_eq!(
        rows(&mut c, "SELECT v FROM dec ORDER BY id").await,
        vec![
            vec![s("1.01")],
            vec![s("2.34")],
            vec![s("-1.01")],
            vec![MVal::NULL]
        ]
    );

    // Metadata and DESCRIBE carry the declared type.
    assert_eq!(
        col_types(&mut c, "SELECT v FROM dec").await,
        vec![MYSQL_TYPE_NEWDECIMAL]
    );
    assert_eq!(
        rows(&mut c, "DESCRIBE dec").await[0][..2],
        vec![s("id"), s("bigint")][..]
    );
    assert_eq!(
        rows(&mut c, "DESCRIBE dec").await[1][..2],
        vec![s("v"), s("decimal(10,2)")][..]
    );

    // Exact comparisons drive a secondary index (cross-scale literal).
    ddl(&mut c, "CREATE INDEX idx_v ON dec (v)").await;
    assert_eq!(
        rows(&mut c, "SELECT id FROM dec WHERE v = 1.010").await,
        vec![vec![s("1")]]
    );
    let rs = rows(&mut c, "EXPLAIN SELECT id FROM dec WHERE v = 1.010").await;
    let MVal::Bytes(b) = &rs[0][0] else {
        panic!("plan line")
    };
    assert_eq!(
        String::from_utf8(b.clone()).unwrap(),
        "IndexScan idx_v -> 1 pks"
    );

    // ORDER BY walks the numeric order, negatives first.
    assert_eq!(
        rows(&mut c, "SELECT v FROM dec WHERE v IS NOT NULL ORDER BY v").await,
        vec![vec![s("-1.01")], vec![s("1.01")], vec![s("2.34")]]
    );

    // SUM stays decimal at the column scale; AVG divides at scale+4.
    assert_eq!(
        rows(&mut c, "SELECT SUM(v) FROM dec").await,
        vec![vec![s("2.34")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT AVG(v) FROM dec").await,
        vec![vec![s("0.780000")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT SUM(v) FROM dec WHERE id = 4").await,
        vec![vec![MVal::NULL]]
    );
    assert_eq!(
        rows(&mut c, "SELECT v + 1 FROM dec WHERE id = 4").await,
        vec![vec![MVal::NULL]]
    );

    // The declared precision is enforced after rounding (MySQL 1292).
    ddl(
        &mut c,
        "CREATE TABLE p5 (id BIGINT PRIMARY KEY, v DECIMAL(5,2))",
    )
    .await;
    c.query_drop("INSERT INTO p5 (id, v) VALUES (1, 999.99)")
        .await
        .expect("edge fits");
    for sql in [
        "INSERT INTO p5 (id, v) VALUES (2, 123456)",
        "INSERT INTO p5 (id, v) VALUES (2, 1000)",
        "UPDATE p5 SET v = 999.995 WHERE id = 1",
    ] {
        let e = mysql_server_error(&mut c, sql).await;
        assert_eq!(e.code, 1292, "{sql}: {}", e.message);
        assert!(e.message.contains("Out of range"), "{sql}: {}", e.message);
    }
    assert_eq!(
        rows(&mut c, "SELECT COUNT(*) FROM p5").await,
        vec![vec![s("1")]]
    );
    node.kill_now();
}
