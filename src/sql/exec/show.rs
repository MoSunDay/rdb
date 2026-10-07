//! SHOW metadata surface: thin catalog/sysvar reads rendered as
//! rowsets -- TABLES / COLUMNS / INDEX / CREATE TABLE / DATABASES /
//! VARIABLES / STATUS (M4). Rendering is pure schema -> text; the
//! sysvar table itself lives in `front::vars` (one source shared with
//! the @@var query path).

use crate::sql::exec::{ColMeta, ExecOutcome, SqlSession, DEFAULT_DB};
use crate::sql::front::vars;
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{Engine, KeyModel, SqlType, TableSchema, Value};
use crate::state::Shared;

/// Server version the SHOW VARIABLES row reports (matches the
/// handshake / `@@version`, `front::shim::SERVER_VERSION`).
const SERVER_VERSION: &str = "8.0.32-rdb";

pub fn run(shared: &Shared, sess: &SqlSession, stmt: &Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::ShowTables => show_tables(shared, sess),
        Statement::ShowColumns(table) => show_columns(shared, table),
        Statement::ShowIndexes(table) => show_indexes(shared, table),
        Statement::ShowCreateTable(table) => show_create_table(shared, table),
        Statement::ShowDatabases => show_databases(sess),
        Statement::ShowVariables { like } => Ok(show_kv(
            "Variable_name",
            vars::show_variables_rows(
                like.as_deref(),
                SERVER_VERSION,
                &vars::SessionVars {
                    isolation: sess.isolation.clone(),
                },
            ),
        )),
        Statement::ShowStatus { like } => Ok(show_kv(
            "Variable_name",
            vars::show_status_rows(like.as_deref()),
        )),
        _ => unreachable!("dispatch maps only SHOW statements here"),
    }
}

/// The two-column Variable_name/Value rowset both VARIABLES and STATUS
/// render (MySQL uses this shape for SHOW STATUS too).
fn show_kv(header: &str, rows: Vec<(String, Value)>) -> ExecOutcome {
    ExecOutcome::Rows {
        columns: vec![
            ColMeta::computed("", header, SqlType::VarChar),
            ColMeta::computed("", "Value", SqlType::VarChar),
        ],
        rows: rows
            .into_iter()
            .map(|(name, value)| vec![Value::Str(name), render_kv_value(value)])
            .collect(),
    }
}

/// SHOW output is text: numbers render in their SQL literal form,
/// strings pass through verbatim (MySQL SHOW cells are never quoted).
fn render_kv_value(v: Value) -> Value {
    Value::Str(match v {
        Value::Null => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Str(s) => s,
        other => format!("{other:?}"),
    })
}

fn show_databases(sess: &SqlSession) -> SqlResult<ExecOutcome> {
    // Single-database model: the implicit default plus the session's
    // USE target when one was picked (deduped, name-order stable).
    let mut names = vec![DEFAULT_DB.to_string()];
    if !sess.db.is_empty() && !sess.db.eq_ignore_ascii_case(DEFAULT_DB) {
        names.push(sess.db.clone());
    }
    Ok(ExecOutcome::Rows {
        columns: vec![ColMeta::computed("", "Database", SqlType::VarChar)],
        rows: names.into_iter().map(|n| vec![Value::Str(n)]).collect(),
    })
}

fn show_create_table(shared: &Shared, table: &str) -> SqlResult<ExecOutcome> {
    let schema = catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))?;
    Ok(ExecOutcome::Rows {
        columns: vec![
            ColMeta::computed("", "Table", SqlType::VarChar),
            ColMeta::computed("", "Create Table", SqlType::VarChar),
        ],
        rows: vec![vec![
            Value::Str(schema.name.clone()),
            Value::Str(render_create_table(&schema)),
        ]],
    })
}

/// Canonical MySQL-style `CREATE TABLE` rendered from the stored
/// schema. Deterministic (declaration-order columns, pk, then indexes
/// by catalog order) and a ROUND TRIP: the rendered text re-parses and
/// rebuilds an equivalent schema (unit-tested below). Engine clause:
/// the row engine reports `InnoDB` (the stock default it behaves as),
/// columnar reports itself (the ENGINE=columnar option).
pub(crate) fn render_create_table(schema: &TableSchema) -> String {
    let q = |s: &str| format!("`{s}`");
    let mut out = format!(
        "CREATE TABLE {} (
",
        q(&schema.name)
    );
    let mut lines = Vec::with_capacity(schema.columns.len() + 1 + schema.indexes.len());
    for (i, c) in schema.columns.iter().enumerate() {
        let mut line = format!("  {} {}", q(&c.name), type_name(c.sql_type));
        let pk_col = schema.pk.iter().any(|p| p.eq_ignore_ascii_case(&c.name));
        // Engine coercion makes pk columns NOT NULL already; render the
        // stored nullability either way (round-trip re-derives it).
        if c.nullable && !(pk_col && schema.key_model != KeyModel::Duplicate) {
            line.push_str(" NULL DEFAULT NULL");
        } else {
            line.push_str(" NOT NULL");
        }
        if schema
            .auto_increment
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(&c.name))
            && i == schema.auto_increment_index().unwrap_or(usize::MAX)
        {
            line.push_str(" AUTO_INCREMENT");
        }
        lines.push(line);
    }
    if !schema.pk.is_empty() && schema.key_model != KeyModel::Duplicate {
        let cols: Vec<String> = schema.pk.iter().map(|p| q(p)).collect();
        lines.push(format!("  PRIMARY KEY ({})", cols.join(", ")));
    }
    for idx in &schema.indexes {
        let kind = if idx.unique { "UNIQUE KEY" } else { "KEY" };
        lines.push(format!("  {} {} ({})", kind, q(&idx.name), q(&idx.column)));
    }
    out.push_str(&lines.join(
        ",
",
    ));
    out.push_str(
        "
)",
    );
    // StarRocks model clauses (schema-recorded): DUPLICATE KEY list
    // and the distribution descriptor. The pre-parser re-lifts both.
    if schema.key_model == KeyModel::Duplicate {
        let cols: Vec<String> = schema.pk.iter().map(|p| q(p)).collect();
        out.push_str(&format!(" DUPLICATE KEY({})", cols.join(", ")));
        if let Some(d) = &schema.distribution {
            let cols: Vec<String> = d.columns.iter().map(|c| q(c)).collect();
            out.push_str(&format!(
                " DISTRIBUTED BY HASH({}) BUCKETS {}",
                cols.join(", "),
                d.buckets
            ));
        }
    }
    let engine = match schema.engine {
        Engine::Columnar => "columnar",
        Engine::Row => "InnoDB",
    };
    out.push_str(&format!(" ENGINE={engine}"));
    out
}

fn show_tables(shared: &Shared, sess: &SqlSession) -> SqlResult<ExecOutcome> {
    let db = if sess.db.is_empty() {
        DEFAULT_DB
    } else {
        sess.db.as_str()
    };
    Ok(ExecOutcome::Rows {
        columns: vec![ColMeta::computed(
            "",
            &format!("Tables_in_{db}"),
            SqlType::VarChar,
        )],
        rows: catalog::list_tables(shared)
            .into_iter()
            .map(|s| vec![Value::Str(s.name)])
            .collect(),
    })
}

fn show_columns(shared: &Shared, table: &str) -> SqlResult<ExecOutcome> {
    let schema = catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))?;
    let str_col = |name: &str| ColMeta::computed("", name, SqlType::VarChar);
    Ok(ExecOutcome::Rows {
        columns: vec![
            str_col("Field"),
            str_col("Type"),
            str_col("Null"),
            str_col("Key"),
            str_col("Default"),
            str_col("Extra"),
        ],
        rows: schema
            .columns
            .iter()
            .map(|c| {
                vec![
                    Value::Str(c.name.clone()),
                    Value::Str(type_name(c.sql_type)),
                    Value::Str(if c.nullable { "YES" } else { "NO" }.to_string()),
                    Value::Str(key_flag(&schema, &c.name).to_string()),
                    Value::Str("NULL".to_string()),
                    Value::Str(String::new()),
                ]
            })
            .collect(),
    })
}

/// MySQL `Key` flag: PRI for every primary-key column (composite pks
/// mark all their columns), UNI/MUL for indexed columns (MUL marks a
/// column whose index is non-unique or shared with the pk --
/// single-column indexes make them the same thing).
fn key_flag(schema: &TableSchema, column: &str) -> &'static str {
    if schema.pk.iter().any(|p| p.eq_ignore_ascii_case(column)) {
        return "PRI";
    }
    match schema
        .indexes
        .iter()
        .find(|i| i.column.eq_ignore_ascii_case(column))
    {
        Some(i) if i.unique => "UNI",
        Some(_) => "MUL",
        None => "",
    }
}

/// SHOW INDEX FROM <table>: one row per index (M2: one column each),
/// MySQL-shaped columns.
fn show_indexes(shared: &Shared, table: &str) -> SqlResult<ExecOutcome> {
    let schema = catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))?;
    let str_col = |name: &str| ColMeta::computed("", name, SqlType::VarChar);
    let int_col = |name: &str| ColMeta::computed("", name, SqlType::Int);
    let mut rows = Vec::new();
    // PRIMARY: one row per pk column in pk order (MySQL numbers the
    // columns of one index from 1 with Seq_in_index). Names come from
    // the column list so the emitted case is canonical.
    for (seq, idx) in schema.pk_indices().into_iter().enumerate() {
        rows.push(vec![
            Value::Str(schema.name.clone()),
            Value::Int(0),
            Value::Str("PRIMARY".to_string()),
            Value::Int(seq as i64 + 1),
            Value::Str(schema.columns[idx].name.clone()),
            Value::Str("BTREE".to_string()),
        ]);
    }
    for i in &schema.indexes {
        rows.push(vec![
            Value::Str(schema.name.clone()),
            // MySQL semantics: Non_unique is 0 for a UNIQUE index and
            // 1 for a plain secondary index.
            Value::Int(!i.unique as i64),
            Value::Str(i.name.clone()),
            Value::Int(1),
            Value::Str(i.column.clone()),
            Value::Str("BTREE".to_string()),
        ]);
    }
    Ok(ExecOutcome::Rows {
        columns: vec![
            str_col("Table"),
            int_col("Non_unique"),
            str_col("Key_name"),
            int_col("Seq_in_index"),
            str_col("Column_name"),
            str_col("Index_type"),
        ],
        rows,
    })
}

/// MySQL-ish type names (narrow v1 domain). DECIMAL spells its full
/// `decimal(p,s)` form (the pair is the type).
/// One row per indexed column, MySQL `SHOW INDEX` shape; used by
/// `show_indexes` (PRIMARY = one row per pk column, `Seq_in_index`
/// counting from 1) and re-used for DDL error messages.
pub(crate) fn type_name(t: SqlType) -> String {
    match t {
        SqlType::Bool => "bool".to_string(),
        SqlType::Int => "bigint".to_string(),
        SqlType::Double => "double".to_string(),
        SqlType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
        SqlType::Date => "date".to_string(),
        SqlType::DateTime => "datetime".to_string(),
        SqlType::VarChar => "varchar".to_string(),
        SqlType::Blob => "blob".to_string(),
    }
}

#[cfg(test)]
#[path = "show_tests.rs"]
mod tests;
