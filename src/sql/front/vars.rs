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

/// Session-scoped values that override the static defaults: whatever
/// the connection last persisted via
/// `SET [SESSION|GLOBAL] TRANSACTION ISOLATION LEVEL ...` (stored
/// hyphenated, e.g. `READ-COMMITTED`). Statement-scoped levels never
/// reach here (rejected at parse).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionVars {
    pub isolation: Option<String>,
}

/// Value of one known system variable (names lowercased by the parser).
pub fn sysvar_value(name: &str, version: &str, session: &SessionVars) -> Option<Value> {
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
        // MySQL's default is REPEATABLE READ and the engine's one
        // isolation (snapshot reads) IS repeatable read -- every
        // accepted level maps onto it. An unset session reports the
        // default; a session-persisted level echoes back verbatim.
        "transaction_isolation" | "tx_isolation" => Some(Value::Str(
            session
                .isolation
                .clone()
                .unwrap_or_else(|| "REPEATABLE-READ".to_string()),
        )),
        // Common ORM/driver probes (M4): values that are true of this
        // server (auto-increment stepping is the engine's own), or the
        // documented static defaults (max_connections has no configured
        // bound; MySQL's stock default 151 is reported verbatim).
        "auto_increment_increment" | "auto_increment_offset" => Some(Value::Int(1)),
        "character_set_database" => Some(Value::Str("utf8mb4".to_string())),
        // Comparisons are bytewise (COMPAT.md): the *_bin collation is
        // the honest name for that.
        "collation_connection" | "collation_server" => Some(Value::Str("utf8mb4_bin".to_string())),
        "init_connect" => Some(Value::Str(String::new())),
        "max_connections" => Some(Value::Int(151)),
        "performance_schema" => Some(Value::Int(0)),
        _ => None,
    }
}

/// Rowset for a parsed @@-query; unknown variables error out like MySQL's
/// `Unknown system variable 'x'` (clients show the message verbatim).
pub fn sysvar_outcome(
    names: &[String],
    version: &str,
    session: &SessionVars,
) -> SqlResult<ExecOutcome> {
    let mut row = Vec::with_capacity(names.len());
    let mut columns = Vec::with_capacity(names.len());
    for name in names {
        let value = sysvar_value(name, version, session).ok_or_else(|| {
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
        columns.push(ColMeta::computed("", name, sql_type));
    }
    Ok(ExecOutcome::Rows {
        columns,
        rows: vec![row],
    })
}

/// Every variable name `sysvar_value` can answer, in stable (sorted)
/// order -- the SHOW VARIABLES row source. Keep in lockstep with the
/// match arms above.
pub fn sysvar_names() -> &'static [&'static str] {
    &[
        "autocommit",
        "auto_increment_increment",
        "auto_increment_offset",
        "character_set_client",
        "character_set_connection",
        "character_set_database",
        "character_set_results",
        "collation_connection",
        "collation_server",
        "init_connect",
        "interactive_timeout",
        "lower_case_table_names",
        "max_allowed_packet",
        "max_connections",
        "net_read_timeout",
        "net_write_timeout",
        "performance_schema",
        "socket",
        "sql_mode",
        "time_zone",
        "transaction_isolation",
        "tx_isolation",
        "version",
        "version_comment",
        "wait_timeout",
    ]
}

/// The SHOW STATUS row source: only counters the server can answer
/// HONESTLY. `Uptime` is wall-clock seconds since process start (the
/// one lazy constant); no load/connection counters are fabricated --
/// there is no connection registry yet, so they are absent rather than
/// hard-coded zeros.
pub fn status_names() -> &'static [&'static str] {
    &["Uptime"]
}

fn process_start() -> std::time::Instant {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *START.get_or_init(std::time::Instant::now)
}

/// Value of one status variable (see [`status_names`]).
pub fn status_value(name: &str) -> Option<Value> {
    match name {
        "Uptime" => Some(Value::Int(
            i64::try_from(process_start().elapsed().as_secs()).unwrap_or(i64::MAX),
        )),
        _ => None,
    }
}

/// SHOW ... LIKE filter, MySQL convention: `_` (one char) and `%`
/// (any run) wildcards over a CASE-INSENSITIVE compare (SHOW-only;
/// data LIKE stays bytewise, gap-matrix decision 3).
pub fn show_like_match(name: &str, pattern: &str) -> bool {
    crate::sql::exec::expr_like::like_match(
        &name.to_ascii_lowercase(),
        &pattern.to_ascii_lowercase(),
    )
}

/// `SHOW [GLOBAL|SESSION] VARIABLES [LIKE 'pat']`: the whole sysvar
/// table (the scope keywords are cosmetic -- variables are static),
/// filtered by the optional pattern.
pub fn show_variables_rows(
    like: Option<&str>,
    version: &str,
    session: &SessionVars,
) -> Vec<(String, Value)> {
    sysvar_names()
        .iter()
        .filter(|n| like.is_none_or(|p| show_like_match(n, p)))
        .filter_map(|n| sysvar_value(n, version, session).map(|v| ((*n).to_string(), v)))
        .collect()
}

/// `SHOW [GLOBAL|SESSION] STATUS [LIKE 'pat']`: the honest status
/// subset over the same filter.
pub fn show_status_rows(like: Option<&str>) -> Vec<(String, Value)> {
    status_names()
        .iter()
        .filter(|n| like.is_none_or(|p| show_like_match(n, p)))
        .filter_map(|n| status_value(n).map(|v| ((*n).to_string(), v)))
        .collect()
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
        let out =
            sysvar_outcome(&names, "8.0.32-rdb", &SessionVars::default()).expect("known vars");
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
        let err =
            sysvar_outcome(&names, "8.0.32-rdb", &SessionVars::default()).expect_err("unknown");
        assert!(err.msg.contains("Unknown system variable"), "{}", err.msg);
    }

    #[test]
    fn isolation_defaults_to_mysql_repeatable_read() {
        for name in ["transaction_isolation", "tx_isolation"] {
            let names = vec![name.to_string()];
            let out =
                sysvar_outcome(&names, "8.0.32-rdb", &SessionVars::default()).expect("known var");
            let ExecOutcome::Rows { rows, .. } = out else {
                panic!("rows");
            };
            // MySQL's default (and the engine's single isolation:
            // snapshot reads = REPEATABLE READ).
            assert_eq!(rows[0][0], Value::Str("REPEATABLE-READ".to_string()));
        }
    }

    #[test]
    fn show_like_match_follows_mysql_convention() {
        // % and _ wildcards, case-insensitive (SHOW-only folding)
        assert!(show_like_match("wait_timeout", "wait%"));
        assert!(show_like_match("wait_timeout", "WAIT%"));
        assert!(show_like_match("version", "VERS%"));
        assert!(show_like_match("version", "%ION"));
        assert!(show_like_match("version", "vers_on"));
        assert!(!show_like_match("version", "vers__on"));
        assert!(!show_like_match("wait_timeout", "net%"));
    }

    #[test]
    fn show_variables_rows_filter_and_completeness() {
        let all = show_variables_rows(None, "8.0.32-rdb", &SessionVars::default());
        // every name the table claims is answerable
        assert_eq!(all.len(), sysvar_names().len());
        // (non-Null values only; SHOW renders them as text at exec)
        // LIKE filters by prefix; values render as stored
        let wait = show_variables_rows(Some("wait%"), "8.0.32-rdb", &SessionVars::default());
        assert_eq!(wait.len(), 1);
        assert_eq!(wait[0].0, "wait_timeout");
        assert_eq!(wait[0].1, Value::Int(28_800));
        // session-scoped value rides through the SHOW path too
        let iso = show_variables_rows(
            Some("transaction_isolation"),
            "8.0.32-rdb",
            &SessionVars {
                isolation: Some("READ-COMMITTED".to_string()),
            },
        );
        assert_eq!(iso[0].1, Value::Str("READ-COMMITTED".to_string()));
        // a filter matching nothing is an empty rowset, not an error
        assert!(
            show_variables_rows(Some("zzz%"), "8.0.32-rdb", &SessionVars::default()).is_empty()
        );
    }

    #[test]
    fn show_status_rows_are_the_honest_subset() {
        let rows = show_status_rows(None);
        assert_eq!(rows.len(), 1, "only answerable counters are listed");
        assert_eq!(rows[0].0, "Uptime");
        assert!(matches!(rows[0].1, Value::Int(_)));
        assert_eq!(show_status_rows(Some("zzz%")).len(), 0);
        assert_eq!(show_status_rows(Some("uptime")).len(), 1, "case-folded");
    }

    #[test]
    fn isolation_round_trips_every_session_level() {
        for level in [
            "READ-UNCOMMITTED",
            "READ-COMMITTED",
            "REPEATABLE-READ",
            "SERIALIZABLE",
        ] {
            let names = vec!["transaction_isolation".to_string()];
            let sess = SessionVars {
                isolation: Some(level.to_string()),
            };
            let out = sysvar_outcome(&names, "8.0.32-rdb", &sess).expect("known var");
            let ExecOutcome::Rows { rows, .. } = out else {
                panic!("rows");
            };
            assert_eq!(
                rows[0][0],
                Value::Str(level.to_string()),
                "{level} must round-trip"
            );
        }
    }
}
