//! The turso side of the table-metadata SHOWs: SHOW TABLES, DESCRIBE /
//! SHOW COLUMNS FROM, SHOW CREATE TABLE. All three read the library's
//! own `sqlite_master` -- rdb keeps no shadow schema. turso's PRAGMA
//! table_info IS supported (T2 smoke, 2026-11-10), so DESCRIBE rides
//! it; the sqlite_master `sql` fallback is not needed.
//!
//! Documented simplifications (ledger-documented with T6):
//! - DESCRIBE Type renders SQLite's declared type lowercased (`integer`
//!   not MySQL's `int`), `blob` for BLOB, and vector columns as
//!   `vector(N)` with the dimension recovered from the sqlite_master
//!   CREATE TABLE text (turso's own parser normalizes `vector(3)` to
//!   bare `vector` in table_info; the bare spelling survives only
//!   when the dim is unrecoverable, e.g. odd external files).
//! - Null renders NO for pk columns even though table_info reports the
//!   raw notnull flag (MySQL's DESCRIBE shape); Key is PRI for pk
//!   columns, empty otherwise (no unique/secondary key reporting).
//! - SHOW CREATE TABLE returns the sqlite_master `sql` text as stored
//!   (SQLite spelling, not MySQL's formatting).

use crate::sql::exec::{ColMeta, ExecOutcome};
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::schema::{SqlType, Value};

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use super::engine::{self, ConnHandle};
use super::translate::{self, declared_vector_dims, quote_ident};

/// SHOW TABLES: every user table in the session's turso library,
/// name-sorted, one `Tables_in_<db>` column.
pub async fn show_tables(sess_db: &str, conn: &ConnHandle) -> SqlResult<ExecOutcome> {
    let sql = "SELECT name FROM sqlite_master \
               WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name";
    let (_, raw) = engine::run_query(conn, sql).await?;
    let cells: Vec<Value> = raw
        .into_iter()
        .map(|r| {
            Value::Str(
                r.into_iter()
                    .next()
                    .map(|v| match v {
                        turso::Value::Text(t) => t,
                        other => format!("{other:?}"),
                    })
                    .unwrap_or_default(),
            )
        })
        .collect();
    Ok(ExecOutcome::Rows {
        columns: vec![ColMeta::computed(
            "",
            &format!("Tables_in_{sess_db}"),
            SqlType::VarChar,
        )],
        rows: cells.into_iter().map(|v| vec![v]).collect(),
    })
}

/// DESCRIBE t / SHOW COLUMNS FROM t over `PRAGMA table_info("t")`
/// (verified supported). A missing table is detected via sqlite_master
/// first -- the PRAGMA itself answers zero rows for unknown names.
pub async fn show_columns(conn: &ConnHandle, table: &str) -> SqlResult<ExecOutcome> {
    if !table_exists(conn, table).await? {
        return Err(SqlError::no_such_table(table));
    }
    // Vector dims for the DESCRIBE Type cell (rocks spells VECTOR(N);
    // table_info carries only the bare `vector`).
    let cols = table_columns(conn, table).await?;
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let (_, raw) = engine::run_query(conn, &sql).await?;
    let str_col = |name: &str| ColMeta::computed("", name, SqlType::VarChar);
    let mut rows = Vec::with_capacity(raw.len());
    for r in raw {
        // Columns: cid, name, type, notnull, dflt_value, pk (by index:
        // the pragma resultset carries no duplicate-name hazards).
        let get = |i: usize| r.get(i).cloned().unwrap_or(turso::Value::Null);
        let name = as_text(&get(1));
        let declared = as_text(&get(2));
        let type_cell = render_declared_type(&declared, cols.dim_of(&name));
        let notnull = matches!(get(3), turso::Value::Integer(n) if n != 0);
        let pk = matches!(get(5), turso::Value::Integer(n) if n > 0);
        let default = match get(4) {
            turso::Value::Null => Value::Str("NULL".to_string()),
            turso::Value::Text(t) => Value::Str(t),
            other => Value::Str(format!("{other:?}")),
        };
        rows.push(vec![
            Value::Str(name),
            Value::Str(type_cell),
            Value::Str(if notnull || pk { "NO" } else { "YES" }.to_string()),
            Value::Str(if pk { "PRI" } else { "" }.to_string()),
            default,
            Value::Str(String::new()),
        ]);
    }
    Ok(ExecOutcome::Rows {
        columns: vec![
            str_col("Field"),
            str_col("Type"),
            str_col("Null"),
            str_col("Key"),
            str_col("Default"),
            str_col("Extra"),
        ],
        rows,
    })
}

/// SHOW CREATE TABLE t: the sqlite_master `sql` text verbatim (SQLite
/// spelling -- a documented deviation from MySQL's formatting).
pub async fn show_create_table(conn: &ConnHandle, table: &str) -> SqlResult<ExecOutcome> {
    let sql = format!(
        "SELECT name, sql FROM sqlite_master WHERE type='table' AND name = '{}'",
        table.replace('\'', "''")
    );
    let (_, raw) = engine::run_query(conn, &sql).await?;
    let Some(first) = raw.into_iter().next() else {
        return Err(SqlError::no_such_table(table));
    };
    let name = first.first().map(as_text_ref).unwrap_or_default();
    let ddl = first
        .get(1)
        .map(as_text_ref)
        .unwrap_or_default()
        .to_string();
    Ok(ExecOutcome::Rows {
        columns: vec![
            ColMeta::computed("", "Table", SqlType::VarChar),
            ColMeta::computed("", "Create Table", SqlType::VarChar),
        ],
        rows: vec![vec![Value::Str(name.to_string()), Value::Str(ddl)]],
    })
}

/// Does the session's library carry a table of this name?
/// (ASCII case-insensitive passthrough: SQLite identifiers are.)
pub async fn table_exists(conn: &ConnHandle, table: &str) -> SqlResult<bool> {
    let sql = format!(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name = '{}' LIMIT 1",
        table.replace('\'', "''")
    );
    let (_, raw) = engine::run_query(conn, &sql).await?;
    Ok(!raw.is_empty())
}

/// One table's column metadata for the vector write path: the column
/// names in table_info (= CREATE TABLE) order, and the declared
/// dimension of every vector-typed column (`None` when unrecoverable
/// from the sqlite_master `sql` text). Plain data, shared by value.
pub struct TableColumns {
    pub names: Vec<String>,
    /// vector column name -> declared dim (None = vector-typed, dim
    /// unknown: validate format only).
    pub vector_dims: HashMap<String, Option<u32>>,
}

impl TableColumns {
    /// The declared dim of column `col`: `Some(None)` = vector-typed
    /// with unknown dim, `None` = not a vector column.
    pub fn dim_of(&self, col: &str) -> Option<Option<u32>> {
        self.vector_dims.get(col).cloned()
    }
}

/// Process-level cache of [`TableColumns`] per (database, table).
/// DDL on the turso path invalidates wholesale ([`invalidate_db`]);
/// the embedded engine is single-process, so no cross-node coherence
/// is needed. One PRAGMA (+ at most one sqlite_master read) per table
/// per DDL generation instead of per write statement.
fn table_columns_cache() -> &'static std::sync::Mutex<HashMap<(String, String), Arc<TableColumns>>>
{
    static CACHE: OnceLock<std::sync::Mutex<HashMap<(String, String), Arc<TableColumns>>>> =
        OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Drop every cached table of `db` (CREATE/DROP/RENAME TABLE land
/// here; the next read repopulates from the library itself).
pub fn invalidate_db(db: &str) {
    let mut cache = table_columns_cache().lock().unwrap();
    cache.retain(|(d, _), _| d != db);
}

/// Column metadata of `table` in the connection's library (missing
/// tables answer empty: the engine's own no-such-table error is the
/// verdict that reaches the client). Vector dims come from the
/// sqlite_master CREATE TABLE text because table_info normalizes
/// `vector(3)` down to bare `vector`.
pub async fn table_columns(conn: &ConnHandle, table: &str) -> SqlResult<Arc<TableColumns>> {
    let key = (conn.db.clone(), table.to_string());
    if let Some(hit) = table_columns_cache().lock().unwrap().get(&key) {
        return Ok(Arc::clone(hit));
    }
    let sql = format!("PRAGMA table_info({})", quote_ident(table));
    let (_, raw) = engine::run_query(conn, &sql).await?;
    let mut names = Vec::with_capacity(raw.len());
    let mut has_vector = false;
    for r in &raw {
        let name = as_text(r.get(1).unwrap_or(&turso::Value::Null));
        let declared = as_text(r.get(2).unwrap_or(&turso::Value::Null));
        has_vector |= translate::is_vector_decl(&declared);
        names.push(name);
    }
    let mut vector_dims = HashMap::new();
    if has_vector {
        let ddl = create_table_sql(conn, table).await?;
        for (name, dim) in declared_vector_dims(&ddl) {
            vector_dims.insert(name, dim);
        }
    }
    let cols = Arc::new(TableColumns { names, vector_dims });
    table_columns_cache()
        .lock()
        .unwrap()
        .insert(key, Arc::clone(&cols));
    Ok(cols)
}

/// The sqlite_master CREATE TABLE text of `table` ("" when absent).
async fn create_table_sql(conn: &ConnHandle, table: &str) -> SqlResult<String> {
    let sql = format!(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name = '{}'",
        table.replace('\'', "''")
    );
    let (_, raw) = engine::run_query(conn, &sql).await?;
    Ok(raw
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
        .map(|v| match v {
            turso::Value::Text(t) => t,
            other => format!("{other:?}"),
        })
        .unwrap_or_default())
}

fn as_text(v: &turso::Value) -> String {
    match v {
        turso::Value::Text(t) => t.clone(),
        turso::Value::Null => String::new(),
        other => format!("{other:?}"),
    }
}

fn as_text_ref(v: &turso::Value) -> &str {
    match v {
        turso::Value::Text(t) => t,
        _ => "",
    }
}

/// The DESCRIBE Type cell: the declared type as table_info reported
/// it, normalized per the documented convention (see module doc). A
/// bare `vector` (turso's own DDL spelling) is re-dimensioned from
/// the CREATE TABLE text so DESCRIBE matches the rocks engine's
/// `vector(N)` -- falling back to the bare spelling only when the dim
/// is unrecoverable (external file, unparseable `sql`).
fn render_declared_type(declared: &str, dim: Option<Option<u32>>) -> String {
    let lower = declared.to_ascii_lowercase();
    if lower == "blob" {
        "blob".to_string()
    } else if lower == "vector" || lower.starts_with("vector(") {
        match dim {
            Some(Some(n)) => format!("vector({n})"),
            // As-declared: `vector(N)` from external SQLite files, or
            // the bare `vector` when the dim did not come back.
            _ => declared.to_string(),
        }
    } else {
        lower
    }
}
