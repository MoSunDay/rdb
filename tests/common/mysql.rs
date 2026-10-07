//! Shared MySQL-client scaffolding of the SQL e2e suites, mounted via
//! `mod common;` + `common::mysql::...`: the connect-with-retry loop,
//! leader-retry DDL/DML, the text-protocol wire-cell row readers,
//! MySQL-errno consts, the per-test single-node worlds, and the
//! per-family fixtures previously living in `sql_funcs_common` /
//! `sql_upsert_common`. New SQL e2e files must NOT re-declare these
//! helpers locally (see plans/2026-10-06-mysql-gap M5).

#![allow(dead_code)]

use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;

pub use mysql_async::consts::ColumnType;

use super::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready, ProcNode};

/// Password every SQL e2e config hands to `root`.
pub const PASS: &str = "e2e-sql-pass";
/// MySQL 1054 (ER_BAD_FIELD_ERROR).
pub const ER_BAD_FIELD_ERROR: u16 = 1054;
/// MySQL 1146 (ER_NO_SUCH_TABLE).
pub const ER_NO_SUCH_TABLE: u16 = 1146;
/// MySQL 1062 (ER_DUP_ENTRY).
pub const ER_DUP_ENTRY: u16 = 1062;
/// MySQL 1064 (ER_PARSE_ERROR).
pub const ER_PARSE_ERROR: u16 = 1064;
/// MySQL 1235 (ER_NOT_SUPPORTED_YET).
pub const ER_NOT_SUPPORTED_YET: u16 = 1235;
/// MySQL 1292 (ER_TRUNCATED_WRONG_VALUE).
pub const ER_TRUNCATED_WRONG_VALUE: u16 = 1292;
/// MySQL 1582 (ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT).
pub const ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT: u16 = 1582;

/// Open a mysql_async connection to the node's MySQL frontend, retrying
/// while the port settles. Retries only IO errors: handshake/auth
/// failures fail fast.
pub async fn connect(node: &ProcNode, user: &str, pass: &str) -> mysql_async::Conn {
    let port = node
        .mysql
        .rsplit(':')
        .next()
        .expect("mysql port")
        .parse::<u16>()
        .expect("mysql port digits");
    let opts = || {
        mysql_async::OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .user(Some(user))
            .pass(Some(pass))
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

/// The default e2e client: `root` with the shared suite password.
pub async fn connect_root(node: &ProcNode) -> mysql_async::Conn {
    connect(node, "root", PASS).await
}

/// Writes (DDL, leader-routed DML) fail with a "leader" error while the
/// bootstrap node is still electing; retry until one of them sticks.
async fn leader_retry(conn: &mut mysql_async::Conn, sql: &str, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if std::time::Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                panic!("{what} {sql}: {e}")
            }
        }
    }
}

/// DDL needs the raft leader; retry until the bootstrap node becomes one.
pub async fn ddl(conn: &mut mysql_async::Conn, sql: &str) {
    leader_retry(conn, sql, "ddl").await
}

/// Leader-routed DML (`run` keeps the historical assert wording of the
/// AUTO_INCREMENT suite, which retries counter-bumping INSERTs too).
pub async fn run(conn: &mut mysql_async::Conn, sql: &str) {
    leader_retry(conn, sql, "run").await
}

/// Rows as raw wire cells (the text protocol hands non-NULL scalars
/// back as bytes; NULLs ride the dedicated variant).
pub async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect()
}

/// Rows sorted by their debug text: suites exercising unspecified
/// arrival order (or ORDER BY itself) compare stably.
pub async fn rows_ordered(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let mut r = rows(conn, sql).await;
    r.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    r
}

/// First cell of the first row of a single-value SELECT.
pub async fn one(conn: &mut mysql_async::Conn, sql: &str) -> MVal {
    rows(conn, sql)
        .await
        .first()
        .unwrap_or_else(|| panic!("no rows for {sql}"))[0]
        .clone()
}

/// First output column of every row.
pub async fn col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<MVal> {
    rows(conn, sql)
        .await
        .into_iter()
        .map(|r| r[0].clone())
        .collect()
}

/// Sorted text cells of a single-column result (order unspecified).
pub async fn sorted_cells(conn: &mut mysql_async::Conn, sql: &str) -> Vec<MVal> {
    let mut cells: Vec<MVal> = col(conn, sql).await;
    cells.sort_by_key(|c| format!("{c:?}"));
    cells
}

/// First row's column wire types (result-metadata assertions).
pub async fn col_types(conn: &mut mysql_async::Conn, sql: &str) -> Vec<ColumnType> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    let row = rs.first().unwrap_or_else(|| panic!("no rows for {sql}"));
    row.columns_ref().iter().map(|c| c.column_type()).collect()
}

/// Poll `SHOW TABLES` until the raft-replicated catalog reaches (or
/// drops) `table`.
pub async fn wait_table(conn: &mut mysql_async::Conn, table: &str, want: bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let names: Vec<String> = rows(conn, "SHOW TABLES")
            .await
            .into_iter()
            .map(|mut r| match r.remove(0) {
                MVal::Bytes(b) => String::from_utf8(b).unwrap(),
                v => panic!("non-bytes table cell {v:?}"),
            })
            .collect();
        if names.iter().any(|n| n == table) == want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "table {table} never reached want={want} (have {names:?})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// The server error a statement failed with (panics on success or on a
/// non-server error).
pub async fn server_error(conn: &mut mysql_async::Conn, sql: &str) -> mysql_async::ServerError {
    super::mysql_server_error(conn, sql).await
}

/// Execute one write and read the connection's affected-rows counter
/// RIGHT AWAY: an erroring statement in between clears it, so probes
/// never interleave with affected assertions.
pub async fn aff(conn: &mut mysql_async::Conn, sql: &str) -> u64 {
    conn.query_drop(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    conn.affected_rows()
}

/// Text cell constructor.
pub fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}

/// Integer cell as the text protocol spells it (`int` is the historical
/// name; `i` is the funcs-suite spelling of the same cell).
pub fn int(v: i64) -> MVal {
    MVal::Bytes(v.to_string().into_bytes())
}

pub fn i(v: i64) -> MVal {
    int(v)
}

fn cell_i(v: &MVal) -> i64 {
    match v {
        MVal::Bytes(b) => std::str::from_utf8(b).unwrap().parse().unwrap(),
        MVal::Int(i) => *i,
        other => panic!("expected int cell, got {other:?}"),
    }
}

fn cell_s(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("expected string cell, got {other:?}"),
    }
}

/// `(int, string)` column pairs of a two-column select.
pub async fn pairs(conn: &mut mysql_async::Conn, sql: &str) -> Vec<(i64, String)> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.iter()
        .map(|r| {
            (
                cell_i(&r.get::<MVal, _>(0).unwrap_or(MVal::NULL)),
                cell_s(&r.get::<MVal, _>(1).unwrap_or(MVal::NULL)),
            )
        })
        .collect()
}

/// `(int, string, string)` triples of a three-column select.
pub async fn triples(conn: &mut mysql_async::Conn, sql: &str) -> Vec<(i64, String, String)> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.iter()
        .map(|r| {
            (
                cell_i(&r.get::<MVal, _>(0).unwrap_or(MVal::NULL)),
                cell_s(&r.get::<MVal, _>(1).unwrap_or(MVal::NULL)),
                cell_s(&r.get::<MVal, _>(2).unwrap_or(MVal::NULL)),
            )
        })
        .collect()
}

/// First column of every row, decoded as i64.
pub async fn ints(conn: &mut mysql_async::Conn, sql: &str) -> Vec<i64> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.iter()
        .map(|r| cell_i(&r.get::<MVal, _>(0).unwrap_or(MVal::NULL)))
        .collect()
}

/// Each `(expression, expected cell)` pair runs as `SELECT <expr>`; the
/// assertion message names the SQL so failures localize.
pub async fn assert_exprs(conn: &mut mysql_async::Conn, cases: &[(&str, MVal)]) {
    for (sql, want) in cases {
        let got = one(conn, &format!("SELECT {sql}")).await;
        assert_eq!(got, *want, "{sql}");
    }
}

/// One fresh bootstrap node with the SQL plane up (per-test: suites run
/// their tests in parallel and each needs an isolated catalog).
pub async fn world(tag: &str) -> (ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-world-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let conn = connect_root(&node).await;
    (node, conn)
}

/// `world` plus the M1 function-suite fixture table `f` spanning the
/// storage type system: BIGINT key, nullable VARCHAR / DOUBLE /
/// DECIMAL(10,3), DATE and DATETIME columns. Rows mix NULLs so
/// three-valued-logic probes have material to work on.
pub async fn funcs_node(tag: &str) -> (ProcNode, mysql_async::Conn) {
    let (node, mut c) = world(&format!("fns-{tag}")).await;
    ddl(
        &mut c,
        "CREATE TABLE f (id BIGINT PRIMARY KEY, name VARCHAR(32), \
         ratio DOUBLE, price DECIMAL(10,3), d DATE, ts DATETIME)",
    )
    .await;
    c.query_drop(
        "INSERT INTO f (id, name, ratio, price, d, ts) VALUES \
         (1, 'ada', 0.5, 10.755, '2024-01-02', '2024-01-02 03:04:05'), \
         (2, 'Bob', 1.25, -2.005, NULL, '2024-02-29 13:05:09'), \
         (3, NULL, NULL, NULL, '2024-03-05', NULL), \
         (4, 'carol', 2.5, 0.125, '2024-03-05', '2024-03-05 00:00:00')",
    )
    .await
    .expect("seed f");
    (node, c)
}
