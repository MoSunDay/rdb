//! Server-system-variable compatibility queries (`SELECT @@var, ...`).
//!
//! Real clients interrogate the server at connect time before issuing any
//! user SQL -- mysql_async sends `SELECT @@max_allowed_packet,
//! @@wait_timeout,@@socket`, the mysql CLI sends `SELECT
//! @@version_comment LIMIT 1`. opensrv answers only the exact
//! single-variable `SELECT @@max_allowed_packet`; every other @@-query
//! reaches `on_query`, where the SQL engine would reject it (no FROM).
//! This module recognizes the narrow `SELECT @@...` shape and answers it
//! from a static table, so the engine never sees fake SQL. Anything
//! outside the shape falls through to the normal parse path.

use crate::sql::exec::{ColMeta, ExecOutcome};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{SqlType, Value};

/// `max_allowed_packet` echoed back; must not exceed opensrv's own answer
/// for the single-variable form (67108864).
pub const MAX_ALLOWED_PACKET: i64 = 67_108_864;

/// Recognize `SELECT @@a, @@b AS x ...` (case-insensitive SELECT, optional
/// trailing `LIMIT n` and `;`). Returns the requested variable names, or
/// `None` when the text is not this exact shape (-> normal parse path).
pub fn parse_sysvar_query(sql: &str) -> Option<Vec<String>> {
    let t = sql.trim().trim_end_matches(';').trim();
    let rest = t
        .strip_prefix("SELECT ")
        .or_else(|| t.strip_prefix("select "))?
        .trim();
    // Drop a trailing LIMIT clause: " limit" followed by digits only.
    let lowered = rest.to_ascii_lowercase();
    let rest = match lowered.rfind(" limit") {
        Some(pos) => {
            let tail = rest[pos + " limit".len()..].trim();
            if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
                &rest[..pos]
            } else {
                rest
            }
        }
        None => rest,
    };
    let mut names = Vec::new();
    for part in rest.split(',') {
        let part = part.trim();
        let body = part.strip_prefix("@@")?;
        // Variable name only; skip any alias (`AS x` / bare alias).
        let name = body.split_whitespace().next()?;
        if name.is_empty() {
            return None;
        }
        names.push(name.to_ascii_lowercase());
    }
    Some(names)
}

/// Value of one known system variable (names lowercased by the parser).
pub fn sysvar_value(name: &str, version: &str) -> Option<Value> {
    match name {
        "max_allowed_packet" => Some(Value::Int(MAX_ALLOWED_PACKET)),
        "wait_timeout" | "interactive_timeout" => Some(Value::Int(28_800)),
        "net_read_timeout" => Some(Value::Int(30)),
        "net_write_timeout" => Some(Value::Int(60)),
        "socket" => Some(Value::Str(String::new())),
        "version" => Some(Value::Str(version.to_string())),
        "version_comment" => Some(Value::Str("rdb".to_string())),
        "autocommit" => Some(Value::Int(1)),
        "sql_mode" => Some(Value::Str(String::new())),
        "time_zone" => Some(Value::Str("SYSTEM".to_string())),
        // Identifier lookups in the engine are case-insensitive.
        "lower_case_table_names" => Some(Value::Int(1)),
        "character_set_client" | "character_set_connection" | "character_set_results" => {
            Some(Value::Str("utf8mb4".to_string()))
        }
        // M1 is autocommit per-statement snapshot reads.
        "transaction_isolation" | "tx_isolation" => Some(Value::Str("READ-COMMITTED".to_string())),
        _ => None,
    }
}

/// Rowset for a parsed @@-query; unknown variables error out like MySQL's
/// `Unknown system variable 'x'` (clients show the message verbatim).
pub fn sysvar_outcome(names: &[String], version: &str) -> SqlResult<ExecOutcome> {
    let mut row = Vec::with_capacity(names.len());
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let value = sysvar_value(name, version).ok_or_else(|| {
            SqlError::new(
                ErrorCode::Unknown,
                format!("Unknown system variable '{name}'"),
            )
        })?;
        let sql_type = match value {
            Value::Int(_) => SqlType::Int,
            _ => SqlType::VarChar,
        };
        row.push(value);
        columns.push(ColMeta {
            table: String::new(),
            name: name.clone(),
            sql_type,
        });
    }
    Ok(ExecOutcome::Rows {
        columns,
        rows: vec![row],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_client_connect_shapes() {
        // mysql_async 0.37 connect probe.
        assert_eq!(
            parse_sysvar_query("SELECT @@max_allowed_packet,@@wait_timeout,@@socket"),
            Some(vec![
                "max_allowed_packet".to_string(),
                "wait_timeout".to_string(),
                "socket".to_string()
            ])
        );
        // mysql CLI login probe (alias + LIMIT + semicolon).
        assert_eq!(
            parse_sysvar_query("SELECT @@version_comment LIMIT 1;"),
            Some(vec!["version_comment".to_string()])
        );
        assert_eq!(
            parse_sysvar_query("select @@VERSION"),
            Some(vec!["version".to_string()])
        );
        assert_eq!(
            parse_sysvar_query("SELECT @@autocommit AS ac"),
            Some(vec!["autocommit".to_string()])
        );
    }

    #[test]
    fn non_sysvar_shapes_fall_through() {
        assert_eq!(parse_sysvar_query("SELECT 1"), None);
        assert_eq!(parse_sysvar_query("SELECT id FROM t"), None);
        assert_eq!(parse_sysvar_query("SET autocommit=1"), None);
        assert_eq!(parse_sysvar_query("SELECT @@a, x"), None);
        assert_eq!(parse_sysvar_query("SHOW VARIABLES LIKE 'x'"), None);
    }

    #[test]
    fn outcome_rows_match_requested_names() {
        let names: Vec<String> = ["max_allowed_packet", "version"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = sysvar_outcome(&names, "8.0.32-rdb").expect("known vars");
        let ExecOutcome::Rows { columns, rows } = out else {
            panic!("rows");
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "max_allowed_packet");
        assert_eq!(rows[0][0], Value::Int(MAX_ALLOWED_PACKET));
        assert_eq!(rows[0][1], Value::Str("8.0.32-rdb".to_string()));
    }

    #[test]
    fn unknown_variable_errors() {
        let names = vec!["no_such_var".to_string()];
        let err = sysvar_outcome(&names, "8.0.32-rdb").expect_err("unknown");
        assert!(err.msg.contains("Unknown system variable"), "{}", err.msg);
    }
}
