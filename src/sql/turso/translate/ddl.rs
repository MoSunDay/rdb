//! DDL rendering for the turso engine: CREATE TABLE (full SqlType
//! mapping, pk/auto-increment, inline secondary indexes as follow-up
//! CREATE INDEX statements), DROP TABLE, CREATE/DROP INDEX. Every
//! shape outside the allowlist rejects with the uniform 1235.

use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::SqlResult;
use crate::sql::storage::schema::{Engine, SqlType};

use super::super::engine;
use super::quote_ident;

/// Render one CREATE TABLE into the statements to run, in order: the
/// CREATE TABLE itself, then one `CREATE [UNIQUE] INDEX` per inline
/// KEY/UNIQUE KEY constraint (index names are unique per library in
/// SQLite but per table in MySQL, so the rendered name is
/// `<table>_<name>` -- a documented DESCRIBE/SHOW INDEX deviation).
pub fn render_create_table(stmt: &Statement) -> SqlResult<Vec<String>> {
    let Statement::CreateTable {
        name,
        if_not_exists,
        columns,
        pk,
        engine: eng,
        starrocks,
        indexes,
    } = stmt
    else {
        return Err(engine::turso_unsupported(
            "non-CREATE TABLE in the CREATE TABLE renderer",
        ));
    };
    if matches!(eng, Engine::Columnar) {
        return Err(engine::turso_unsupported(
            "ENGINE=columnar tables (columnar is the host engine's table model)",
        ));
    }
    if starrocks.is_some() {
        return Err(engine::turso_unsupported(
            "StarRocks-model tables (PRIMARY KEY/DUPLICATE KEY models)",
        ));
    }
    if columns.is_empty() {
        return Err(engine::turso_unsupported("CREATE TABLE without columns"));
    }
    // AUTO_INCREMENT: exactly one, on THE single int pk column -- the
    // SQLite `INTEGER PRIMARY KEY AUTOINCREMENT` shape. Anything else
    // is MySQL's 1075-shaped reject here.
    if let Some(c) = columns.iter().find(|c| c.auto_increment) {
        let is_single_int_pk =
            pk.len() == 1 && pk[0].eq_ignore_ascii_case(&c.name) && c.sql_type == SqlType::Int;
        if !is_single_int_pk {
            return Err(engine::turso_unsupported(format!(
                "AUTO_INCREMENT on column '{}' (must be the single INTEGER PRIMARY KEY column)",
                c.name
            )));
        }
    }
    let mut defs = Vec::with_capacity(columns.len() + 1);
    for c in columns {
        let mut def = format!("{} {}", quote_ident(&c.name), sqlite_type(&c.sql_type)?);
        if !c.nullable {
            def.push_str(" NOT NULL");
        }
        if pk.len() == 1 && pk[0].eq_ignore_ascii_case(&c.name) {
            if c.auto_increment {
                def.push_str(" PRIMARY KEY AUTOINCREMENT");
            } else {
                def.push_str(" PRIMARY KEY");
            }
        }
        defs.push(def);
    }
    if pk.len() > 1 {
        let cols = pk
            .iter()
            .map(|p| quote_ident(p))
            .collect::<Vec<_>>()
            .join(", ");
        defs.push(format!("PRIMARY KEY ({cols})"));
    }
    let mut out = vec![format!(
        "CREATE TABLE {}{} ({})",
        quote_ident(name),
        if *if_not_exists { " IF NOT EXISTS" } else { "" },
        defs.join(", ")
    )];
    for idx in indexes {
        out.push(format!(
            "CREATE {}INDEX IF NOT EXISTS {} ON {} ({})",
            if idx.unique { "UNIQUE " } else { "" },
            quote_ident(&format!("{}_{}", name, idx.name)),
            quote_ident(name),
            quote_ident(&idx.column)
        ));
    }
    Ok(out)
}

/// `DROP TABLE [IF EXISTS] t`.
pub fn render_drop_table(name: &str, if_exists: bool) -> String {
    format!(
        "DROP TABLE {}{}",
        if if_exists { "IF EXISTS " } else { "" },
        quote_ident(name)
    )
}

/// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON t (col)`: single
/// column, straight passthrough (the IR only ever carries one).
pub fn render_create_index(stmt: &Statement) -> SqlResult<String> {
    let Statement::CreateIndex {
        table,
        name,
        column,
        unique,
        if_not_exists,
    } = stmt
    else {
        return Err(engine::turso_unsupported(
            "non-CREATE INDEX in the CREATE INDEX renderer",
        ));
    };
    Ok(format!(
        "CREATE {}INDEX {}{} ON {} ({})",
        if *unique { "UNIQUE " } else { "" },
        if *if_not_exists { "IF NOT EXISTS " } else { "" },
        quote_ident(name),
        quote_ident(table),
        quote_ident(column)
    ))
}

/// `DROP INDEX [IF EXISTS] name ON t`: MySQL scopes index names per
/// table, SQLite per library -- the rendered statement drops by name
/// only (the ON t half is unrecoverable without a schema probe).
pub fn render_drop_index(stmt: &Statement) -> SqlResult<String> {
    let Statement::DropIndex {
        name, if_exists, ..
    } = stmt
    else {
        return Err(engine::turso_unsupported(
            "non-DROP INDEX in the DROP INDEX renderer",
        ));
    };
    Ok(format!(
        "DROP INDEX {}{}",
        if *if_exists { "IF EXISTS " } else { "" },
        quote_ident(name)
    ))
}

/// MySQL column type (the storage layer's narrow SqlType set) ->
/// SQLite declared type. Every SqlType variant maps or the whole
/// CREATE TABLE rejects.
///
/// Deviations carried by the ledger: DECIMAL loses exact-decimal
/// affinity (SQLite has none -- REAL storage, float arithmetic);
/// DATE/DATETIME/TIMESTAMP ride TEXT in their canonical civil
/// spellings; VECTOR(N) keeps its declared spelling verbatim
/// (lowercase `vector(n)`) so DESCRIBE round-trips table_info and T4
/// can layer the vector semantics -- the engine itself stores BLOB.
fn sqlite_type(t: &SqlType) -> SqlResult<String> {
    Ok(match t {
        SqlType::Bool | SqlType::Int => "INTEGER".to_string(),
        SqlType::Double => "REAL".to_string(),
        // ledger: DECIMAL(p,s) -> REAL (no exact decimals in SQLite).
        SqlType::Decimal { .. } => "REAL".to_string(),
        SqlType::VarChar => "TEXT".to_string(),
        SqlType::Date | SqlType::DateTime => "TEXT".to_string(),
        SqlType::Blob => "BLOB".to_string(),
        // Verbatim lowercase `vector(n)` (T4 convention): round-trips
        // through table_info for DESCRIBE; the stored values are the
        // engine's vector-serialized BLOBs either way.
        SqlType::Vector { dim } => format!("vector({dim})"),
    })
}
