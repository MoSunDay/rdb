//! Dataset preparation for the mysql workloads: create + pre-fill the
//! per-workload tables once, before any client connects. One setup
//! connection does everything, retrying while the single bootstrap node
//! finishes electing its leader (writes before that answer "leader").
//!
//! Shapes (03-perf-study.md §5.2): `bench_point` / `bench_range` /
//! `bench_knn` hold `--rows` rows (default 10k); the join pair holds
//! `a = rows/10` rows over `rows/10` distinct k values and `b = rows`
//! rows over the same k domain, so one a row matches ~10 b rows.

use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use crate::cli::{Config, Workload};
use crate::mysql;

/// Rows per multi-row INSERT (statement text stays ~tens of KB).
const INSERT_BATCH: u64 = 500;
/// How long setup waits for the port to settle and the leader to appear.
const RETRY_SECS: u64 = 20;

/// Tables the workload needs, with the row count each must hold.
fn tables(cfg: &Config) -> Vec<(&'static str, u64)> {
    match cfg.workload {
        Workload::MysqlPoint => vec![("bench_point", cfg.rows as u64)],
        Workload::MysqlRange => vec![("bench_range", cfg.rows as u64)],
        Workload::MysqlJoin => vec![
            ("bench_join_a", (cfg.rows as u64 / 10).max(1)),
            ("bench_join_b", cfg.rows as u64),
        ],
        Workload::MysqlKnnScan => vec![("bench_knn", cfg.rows as u64)],
        _ => Vec::new(),
    }
}

/// Drop + create every table of the workload (idempotent reruns).
fn ddl_statements(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for (table, _) in tables(cfg) {
        out.push(format!("DROP TABLE IF EXISTS {table}"));
        let columns = match table {
            "bench_point" => "(pk BIGINT PRIMARY KEY, c1 VARCHAR(64), c2 BIGINT, c3 DOUBLE)",
            "bench_range" => "(pk BIGINT PRIMARY KEY, c1 VARCHAR(64), c2 BIGINT)",
            "bench_join_a" | "bench_join_b" => "(pk BIGINT PRIMARY KEY, k BIGINT)",
            _ => "(pk BIGINT PRIMARY KEY)",
        };
        out.push(format!("CREATE TABLE {table} {columns}"));
    }
    out
}

/// A fixed-width text column value: `"v"` + pk zero-padded, so row
/// payloads are realistic in size and deterministic across reruns.
fn text_value(pk: u64) -> String {
    format!("v{pk:031}")
}

/// One row's VALUES tuple for the table.
fn values_tuple(table: &str, k_card: u64, pk: u64) -> String {
    match table {
        "bench_point" => format!("({pk}, '{}', {}, {}.5)", text_value(pk), pk % 997, pk % 97),
        "bench_range" => format!("({pk}, '{}', {})", text_value(pk), pk % 997),
        "bench_join_a" | "bench_join_b" => {
            let k = if k_card <= 1 { 1 } else { pk % k_card + 1 };
            format!("({pk}, {k})")
        }
        _ => format!("({pk})"),
    }
}

/// Multi-row INSERTs covering 1..=rows of every table.
fn insert_statements(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for (table, rows) in tables(cfg) {
        let k_card = (cfg.rows as u64 / 10).max(1);
        for start in (1..=rows).step_by(INSERT_BATCH as usize) {
            let end = (start + INSERT_BATCH - 1).min(rows);
            let tuples: Vec<String> = (start..=end)
                .map(|pk| values_tuple(table, k_card, pk))
                .collect();
            let columns = if table.ends_with("join_a") || table.ends_with("join_b") {
                "(pk, k)"
            } else if table == "bench_point" {
                "(pk, c1, c2, c3)"
            } else if table == "bench_range" {
                "(pk, c1, c2)"
            } else {
                "(pk)"
            };
            out.push(format!(
                "INSERT INTO {table} {columns} VALUES {}",
                tuples.join(", ")
            ));
        }
    }
    out
}

/// Connect with retry while the mysql port settles (refused/ECONNRESET
/// class failures carry the "io:" prefix set by `mysql::connect`).
async fn connect_retry(cfg: &Config) -> Result<TcpStream, String> {
    let deadline = Instant::now() + Duration::from_secs(RETRY_SECS);
    loop {
        match mysql::connect(&cfg.addr, &cfg.mysql_user, &cfg.mysql_pass).await {
            Ok(sock) => return Ok(sock),
            Err(text) if text.starts_with("io:") && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(text) => return Err(format!("connect {addr}: {text}", addr = cfg.addr)),
        }
    }
}

/// Run one DDL/INSERT, retrying while the node still answers "leader".
async fn exec_retry(sock: &mut TcpStream, sql: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(RETRY_SECS);
    loop {
        match mysql::query(sock, sql).await {
            Ok(_) => return Ok(()),
            Err(text) if text.contains("leader") && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(300)).await
            }
            Err(text) => return Err(format!("setup statement failed ({sql:.60}...): {text}")),
        }
    }
}

/// Create and populate the workload tables, then verify the row counts.
pub async fn ensure_dataset(cfg: &Config) -> Result<(), String> {
    let mut sock = connect_retry(cfg).await?;
    for sql in ddl_statements(cfg) {
        exec_retry(&mut sock, &sql).await?;
    }
    for sql in insert_statements(cfg) {
        exec_retry(&mut sock, &sql).await?;
    }
    for (table, want) in tables(cfg) {
        let got =
            mysql::query_first_field(&mut sock, &format!("SELECT COUNT(*) FROM {table}")).await?;
        let got: u64 = got
            .parse()
            .map_err(|_| format!("COUNT(*) of {table} is not a number: {got}"))?;
        if got != want {
            return Err(format!("{table} holds {got} rows, expected {want}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(workload: Workload, rows: usize) -> Config {
        Config {
            addr: "h:1".to_string(),
            token: String::new(),
            clients: 1,
            duration: 1,
            pipeline: 1,
            workload,
            host: String::new(),
            topic: String::new(),
            batch: 0,
            rows,
            key_range: rows,
            span: 100,
            mysql_user: "root".to_string(),
            mysql_pass: String::new(),
        }
    }

    #[test]
    fn point_ddl_and_inserts() {
        let c = cfg(Workload::MysqlPoint, 1200);
        let ddl = ddl_statements(&c);
        assert_eq!(ddl[0], "DROP TABLE IF EXISTS bench_point");
        assert_eq!(
            ddl[1],
            "CREATE TABLE bench_point (pk BIGINT PRIMARY KEY, c1 VARCHAR(64), c2 BIGINT, c3 DOUBLE)"
        );
        // 1200 rows -> 500 + 500 + 200 across three INSERTs.
        let inserts = insert_statements(&c);
        assert_eq!(inserts.len(), 3);
        assert!(inserts[0].starts_with("INSERT INTO bench_point (pk, c1, c2, c3) VALUES (1, 'v"));
        assert!(inserts[0].ends_with("')") || inserts[0].contains(", 2, "));
        assert!(inserts[2].starts_with("INSERT INTO bench_point (pk, c1, c2, c3) VALUES (1001, 'v"));
    }

    #[test]
    fn join_tables_split_one_to_ten() {
        let c = cfg(Workload::MysqlJoin, 10_000);
        assert_eq!(
            tables(&c),
            vec![("bench_join_a", 1_000), ("bench_join_b", 10_000)]
        );
        // k stays inside the 1k cardinality on both sides.
        assert_eq!(values_tuple("bench_join_a", 1_000, 1), "(1, 2)");
        assert_eq!(values_tuple("bench_join_b", 1_000, 1_000), "(1000, 1)");
        assert_eq!(values_tuple("bench_join_b", 1_000, 2_345), "(2345, 346)");
        let ddl = ddl_statements(&c);
        assert!(ddl
            .contains(&"CREATE TABLE bench_join_a (pk BIGINT PRIMARY KEY, k BIGINT)".to_string()));
    }

    #[test]
    fn knn_table_is_pk_only() {
        let c = cfg(Workload::MysqlKnnScan, 50);
        assert_eq!(
            ddl_statements(&c),
            vec![
                "DROP TABLE IF EXISTS bench_knn".to_string(),
                "CREATE TABLE bench_knn (pk BIGINT PRIMARY KEY)".to_string(),
            ]
        );
        assert_eq!(insert_statements(&c).len(), 1);
    }
}
