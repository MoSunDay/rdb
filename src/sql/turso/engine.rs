//! turso engine lifecycle: the config gate, per-database library
//! routing, the process-level `Database` handle cache and per-session
//! pinned `Connection`s (one per (session, database), held across
//! statements so explicit transactions later ride a single engine
//! connection). Free functions + plain data only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use tokio::sync::Mutex;

use crate::conf::Config;
use crate::sql::exec::SqlSession;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};

/// Database-name prefix marking a turso-engine library. Convention is
/// THE engine marker: stock sqlparser `CreateDatabase` has no ENGINE
/// field, so the name carries the routing decision.
pub const TURSO_DB_PREFIX: &str = "turso_";

/// The engine answers iff both keys are set: `turso_engine: true` alone
/// (no directory) or a directory alone changes nothing.
pub fn enabled(conf: &Config) -> bool {
    conf.turso_engine && !conf.turso_db_dir.is_empty()
}

/// Case-sensitive on purpose (MySQL db names are case-sensitive on
/// Linux); the host default db keeps its case-blind check.
pub fn is_turso_db(name: &str) -> bool {
    name.starts_with(TURSO_DB_PREFIX)
}

/// Filesystem path of one turso library: `<turso_db_dir>/<name>.db`.
pub fn db_path(conf: &Config, name: &str) -> PathBuf {
    PathBuf::from(&conf.turso_db_dir).join(format!("{name}.db"))
}

/// A session's pinned engine connection. Wrapper only: turso's
/// `Connection` has no Debug impl, and the owning database name rides
/// along so a `USE` across turso databases re-pins instead of silently
/// reusing the previous library's connection. Operated on by the free
/// functions below; no methods.
pub struct ConnHandle {
    /// Logical database name the connection is pinned to.
    pub db: String,
    /// The engine connection itself.
    pub conn: turso::Connection,
}

impl std::fmt::Debug for ConnHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnHandle").field("db", &self.db).finish()
    }
}

/// Process-level cache of open library handles. `Database` is a cheap
/// Clone around an Arc core; one handle per library file. A tokio mutex
/// (not std) on purpose: the guard spans the async build below, so two
/// sessions racing on one library still end up with a single handle.
fn databases() -> &'static Mutex<HashMap<String, turso::Database>> {
    static DBS: OnceLock<Mutex<HashMap<String, turso::Database>>> = OnceLock::new();
    DBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Open (or fetch the cached) `Database` for `name`. The turso builder
/// creates the library file when missing (rw open, default flags). An
/// EXISTING file whose permission bits deny every write decides the
/// read-only open mode (research doc item 3): the mode bits are read
/// directly rather than probing with an open(2), because a privileged
/// process (root, as in the e2e harness) bypasses permission checks
/// and would otherwise keep writing a 0444 library.
pub async fn open_database(conf: &Config, name: &str) -> Result<turso::Database, turso::Error> {
    let mut cache = databases().lock().await;
    if let Some(db) = cache.get(name) {
        return Ok(db.clone());
    }
    let path = db_path(conf, name);
    if let Err(e) = std::fs::create_dir_all(&conf.turso_db_dir) {
        return Err(turso::Error::IoError(e.kind(), "create turso_db_dir"));
    }
    let mut builder = turso::Builder::new_local(&path.to_string_lossy());
    if path.is_file() && file_is_readonly(&path) {
        builder = builder.read_only(true);
    }
    let db = builder.build().await?;
    cache.insert(name.to_string(), db.clone());
    Ok(db)
}

/// True when the file's permission bits carry no write permission for
/// anyone (`mode & 0o222 == 0`): the filesystem's own read-only
/// verdict, independent of who is asking. Non-unix targets have no
/// such verdict and always open read-write.
fn file_is_readonly(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(path) {
            Ok(m) => m.mode() & 0o222 == 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// Forget the cached `Database` handle of `name` (DROP DATABASE): the
/// next open rebuilds from the (replaced or absent) file, so a dropped
/// library can never serve stale pages from the old inode.
pub async fn evict_database(name: &str) {
    databases().lock().await.remove(name);
}

/// Get-or-create the session's pinned connection to `name`, storing it
/// on `sess.turso`. A pinned connection to a DIFFERENT database is
/// replaced (one connection per (session, database) at any time).
pub async fn session_conn(
    conf: &Config,
    sess: &mut SqlSession,
    name: &str,
) -> SqlResult<Arc<Mutex<ConnHandle>>> {
    if let Some(existing) = sess.turso.as_ref() {
        if existing.lock().await.db == name {
            return Ok(Arc::clone(existing));
        }
    }
    let db = open_database(conf, name).await.map_err(map_error)?;
    let conn = db.connect().map_err(map_error)?;
    // 5s busy timeout on every pinned connection: a second session's
    // write while this one holds the engine's single writer lock then
    // degrades to MySQL 1205 after a phased (1ms..100ms) wait instead
    // of failing instantly -- concurrent turso sessions get the same
    // "wait then 1205" shape the host engine answers for latch
    // contention (see Error::Busy in map_error).
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(map_error)?;
    let handle = Arc::new(Mutex::new(ConnHandle {
        db: name.to_string(),
        conn,
    }));
    sess.turso = Some(Arc::clone(&handle));
    Ok(handle)
}

/// True when the pinned engine connection sits inside an open explicit
/// transaction (turso flips its autocommit flag on BEGIN; the host's
/// `sess.txn` state machine never engages for turso libraries, so the
/// connection itself is the transaction ledger).
pub async fn in_txn(conn: &ConnHandle) -> SqlResult<bool> {
    Ok(!conn.conn.is_autocommit().map_err(map_error)?)
}

/// Guard for `USE <db>` while a turso transaction is open: switching
/// to a DIFFERENT database would silently re-pin the session onto
/// another library while the old one's transaction dangles, so it
/// answers 1235 with the escape hatch spelled out. Same-db USE
/// (case variants) passes; sessions without a pinned connection (no
/// open transaction by construction) and host-engine sessions pass
/// unchanged.
pub async fn check_use(sess: &SqlSession, new_db: &str) -> SqlResult<()> {
    let Some(handle) = sess.turso.as_ref() else {
        return Ok(());
    };
    let conn = handle.lock().await;
    if !in_txn(&conn).await? {
        return Ok(());
    }
    if conn.db.eq_ignore_ascii_case(new_db) {
        return Ok(());
    }
    Err(SqlError::unsupported(
        "switching database inside a turso transaction (COMMIT or ROLLBACK first)",
    ))
}

/// Run one read-shaped statement, collecting column metadata (name +
/// declared type -- the honest typing for real table columns) and
/// every row. The Rows are drained to completion (turso snapshot
/// stickiness: an undrained Rows keeps the connection inside its read
/// transaction, hiding later commits from it).
pub async fn run_query(
    conn: &ConnHandle,
    sql: &str,
) -> SqlResult<(Vec<(String, Option<String>)>, Vec<Vec<turso::Value>>)> {
    let mut rows = conn.conn.query(sql, ()).await.map_err(map_error)?;
    let columns = rows
        .columns()
        .into_iter()
        .map(|c| (c.name().to_string(), c.decl_type().map(str::to_string)))
        .collect();
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(map_error)? {
        let mut cells = Vec::with_capacity(row.column_count());
        for i in 0..row.column_count() {
            cells.push(row.get_value(i).map_err(map_error)?);
        }
        out.push(cells);
    }
    Ok((columns, out))
}

/// Run one write-shaped statement; returns the affected row count. A
/// read-only library's write attempts surface as MySQL 1290 with the
/// library named (the open mode itself was decided by the file's
/// permission bits, see [`open_database`]).
pub async fn run_execute(conn: &ConnHandle, sql: &str) -> SqlResult<u64> {
    conn.conn.execute(sql, ()).await.map_err(|e| match &e {
        turso::Error::Readonly(_) => SqlError::new(
            ErrorCode::ReadOnly,
            format!("turso database file is read-only: {}.db", conn.db),
        ),
        _ => map_error(e),
    })
}

/// turso -> MySQL error mapping: variant-first (the research table),
/// string-contains fallbacks on the undifferentiated `Error(String)`
/// variant. Turso's SQLite-family spellings carry MySQL errnos so
/// parity with the host engine's own rejections holds.
pub fn map_error(e: turso::Error) -> SqlError {
    match &e {
        // "UNIQUE constraint failed: t.col" -> MySQL 1062 with the
        // offending key echoed (the duplicate VALUE is engine-side;
        // the key name is what both engines can name).
        turso::Error::Constraint(m) if m.starts_with("UNIQUE constraint failed") => {
            let key = m
                .trim_start_matches("UNIQUE constraint failed: ")
                .trim()
                .to_string();
            SqlError::new(
                ErrorCode::DupEntry,
                format!("Duplicate entry for key '{key}'"),
            )
        }
        // NOT NULL violations are MySQL 1048, not a duplicate entry.
        turso::Error::Constraint(m) if m.contains("NOT NULL constraint failed") => {
            let col = m
                .split_once("NOT NULL constraint failed: ")
                .map(|(_, c)| c.trim().to_string())
                .unwrap_or_else(|| m.clone());
            SqlError::new(ErrorCode::BadNull, format!("Column '{col}' cannot be null"))
        }
        turso::Error::Constraint(m) => SqlError::new(ErrorCode::DupEntry, m.clone()),
        turso::Error::Busy(m) | turso::Error::BusySnapshot(m) => {
            SqlError::new(ErrorCode::LockWaitTimeout, m.clone())
        }
        turso::Error::Readonly(m) => SqlError::new(ErrorCode::ReadOnly, m.clone()),
        // A read-only open of a missing library file: MySQL's unknown
        // database (the catalog never saw it).
        turso::Error::IoError(std::io::ErrorKind::NotFound, _) => SqlError::new(
            ErrorCode::UnknownDb,
            "turso database file not found".to_string(),
        ),
        // The internal single-row helper never surfaces through
        // run_query/run_execute; keep it loud and recognizable.
        turso::Error::QueryReturnedNoRows => SqlError::new(
            ErrorCode::Unknown,
            "internal: turso QueryReturnedNoRows escaped".to_string(),
        ),
        turso::Error::Error(m) => {
            if m.contains("already exists") {
                SqlError::new(ErrorCode::TableExists, m.clone())
            } else if m.contains("no such table") {
                SqlError::new(ErrorCode::NoSuchTable, m.clone())
            } else if m.contains("no such column") {
                SqlError::new(ErrorCode::BadField, m.clone())
            } else if m.contains("syntax error") {
                SqlError::parse(m.clone())
            } else if m
                // Defensive only: the BEGIN arm emulates MySQL's
                // implicit-commit semantics, so a nested BEGIN never
                // reaches the engine (it commits first, then begins).
                // If it ever leaks through, it maps to the host's
                // in-transaction DDL errno shape (1235) with the
                // engine's own message kept verbatim.
                .contains("cannot start a transaction within a transaction")
            {
                SqlError::new(ErrorCode::TxnDdl, m.clone())
            } else if m.contains("Vectors must have the same dimensions")
                || m.contains("same type")
                // vector32()/vector_extract() rejecting a malformed
                // '[e,e,...]' text: the same MySQL 1292 the rocks
                // engine answers for TO_VECTOR('nope') (the message
                // text stays the engine's own spelling -- ledger).
                || m.contains("Invalid vector value")
            {
                // T4: vector dimension/type/value mismatches are
                // MySQL's 1292 wrong-value family.
                SqlError::new(ErrorCode::WrongValue, m.clone())
            } else {
                SqlError::new(ErrorCode::Unknown, m.clone())
            }
        }
        other => SqlError::new(ErrorCode::Unknown, other.to_string()),
    }
}

/// The uniform "engine-bound but not (yet) supported" rejection:
/// errno 1235 with the COMPAT.sql.md pointer.
pub fn turso_unsupported(what: impl std::fmt::Display) -> SqlError {
    SqlError::unsupported(format!("{what} on turso engine (see COMPAT.sql.md)"))
}
