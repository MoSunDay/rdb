//! Database-object surface of the turso engine: the SHOW DATABASES
//! merge (library-file listing), USE validation (file existence), and
//! CREATE/DROP DATABASE -- each CREATE/DROP mirroring one raft-catalog
//! existence entry (`sql_db/<name>` -> engine marker, the
//! `sql_sequence/` precedent) next to the node-local library file
//! (physical data is not raft-replicated, exactly like host rows).

use crate::sql::exec::ddl::{catalog_apply, CatalogMutation};
use crate::sql::exec::{ExecOutcome, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::state::Shared;

use super::engine;

/// Every turso library visible in `<turso_db_dir>` (suffix stripped),
/// name-sorted. The SHOW DATABASES merge reads this; a file that
/// pre-dated its catalog entry (crash between the two writes) is still
/// listed -- the file is the engine's ground truth.
pub fn list_dbs(conf: &crate::conf::Config) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&conf.turso_db_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".db") {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// USE validation for turso names: the library file must exist (rw or
/// read-only). Auto-creation happens ONLY via CREATE DATABASE, so USE
/// of a never-created name answers the frontend's unknown-database
/// error instead of fabricating an empty library.
pub fn db_file_exists(conf: &crate::conf::Config, name: &str) -> bool {
    engine::db_path(conf, name).is_file()
}

/// A creatable/droppable database name: `turso_` prefix (the engine
/// marker) and identifier-safe chars only -- no path tricks, no
/// quoting surprises in file names or FSM keys.
/// The name half of [`validate_db_name`] alone: identifier-safe chars
/// only -- no path tricks, no quoting surprises in file names or FSM
/// keys (DROP runs it after its own rdb/prefix handling).
pub fn validate_db_chars(name: &str) -> SqlResult<()> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(SqlError::parse(format!(
            "invalid database name '{name}' (allowed: [A-Za-z0-9_])"
        )));
    }
    Ok(())
}

/// A creatable database name: `turso_` prefix (the engine marker) and
/// identifier-safe chars.
pub fn validate_db_name(name: &str) -> SqlResult<()> {
    if !engine::is_turso_db(name) {
        return Err(SqlError::unsupported(format!(
            "CREATE/DROP DATABASE '{name}': only turso_* (turso engine) databases exist as \
             explicit objects; 'rdb' is implicit"
        )));
    }
    validate_db_chars(name)
}

/// The raft-catalog existence entry of `name` (engine marker), read
/// through the FSM view like every catalog read. Empty = absent
/// (never written, or tombstoned).
fn db_entry(shared: &Shared, name: &str) -> String {
    let guard = shared.raft.read().unwrap();
    crate::state::raft_get(&guard, &catalog::db_key(name))
}

/// `CREATE DATABASE [IF NOT EXISTS] turso_x`: one raft-catalog entry
/// (leader-only, the `sql_sequence/` queue_put_kv precedent) plus the
/// empty library file via the T1 open path (builder auto-creates).
pub async fn create_database(
    shared: &Shared,
    name: &str,
    if_not_exists: bool,
) -> SqlResult<ExecOutcome> {
    if !engine::enabled(&shared.conf) {
        return Err(SqlError::unsupported(
            "CREATE DATABASE (turso engine disabled on this node)",
        ));
    }
    validate_db_name(name)?;
    if db_file_exists(&shared.conf, name) {
        if if_not_exists {
            return Ok(ExecOutcome::Ok);
        }
        return Err(SqlError::new(
            ErrorCode::DbCreateExists,
            format!("Can't create database '{name}'; database exists"),
        ));
    }
    catalog_apply(
        shared,
        CatalogMutation::Kv {
            key: catalog::db_key(name),
            value: catalog::SQL_DB_TURSO.to_string(),
        },
    )
    .await?;
    // After the replicated entry lands: create the empty library file
    // (rw mode -- a fresh file is writable by definition).
    engine::open_database(&shared.conf, name)
        .await
        .map_err(engine::map_error)?;
    Ok(ExecOutcome::Ok)
}

/// `DROP DATABASE [IF EXISTS] turso_x`: tombstone the catalog entry
/// when present, evict cached engine handles, delete the library file
/// and its `-wal`/`-shm`/`-journal` siblings. MySQL semantics: the
/// session may keep pointing at the dropped name (a later USE fails
/// with the unknown-database error at the frontend gate; engine
/// statements fail at file open).
pub async fn drop_database(
    shared: &Shared,
    sess: &mut SqlSession,
    name: &str,
    if_exists: bool,
) -> SqlResult<ExecOutcome> {
    if !engine::enabled(&shared.conf) {
        return Err(SqlError::unsupported(
            "DROP DATABASE (turso engine disabled on this node)",
        ));
    }
    if name.eq_ignore_ascii_case(crate::sql::exec::DEFAULT_DB) {
        // The host database is implicit, not an object: refuse
        // outright (MySQL refuses dropping its own system db too).
        return Err(SqlError::unsupported("DROP DATABASE 'rdb'"));
    }
    if !engine::is_turso_db(name) {
        // No other explicit databases exist: a non-turso name is by
        // definition a database that doesn't exist (MySQL 1008; with
        // IF EXISTS it is the standard no-op).
        if if_exists {
            return Ok(ExecOutcome::Ok);
        }
        return Err(SqlError::new(
            ErrorCode::DbDropExists,
            format!("Can't drop database '{name}'; database doesn't exist"),
        ));
    }
    validate_db_chars(name)?;
    let entry = db_entry(shared, name);
    let file_present = db_file_exists(&shared.conf, name);
    if entry.is_empty() && !file_present && !if_exists {
        return Err(SqlError::new(
            ErrorCode::DbDropExists,
            format!("Can't drop database '{name}'; database doesn't exist"),
        ));
    }
    if !entry.is_empty() {
        catalog_apply(
            shared,
            CatalogMutation::Kv {
                key: catalog::db_key(name),
                // Empty value = the standard FSM tombstone (readers
                // treat it as absent; queue_drop's precedent).
                value: String::new(),
            },
        )
        .await?;
    }
    // Evict before unlinking: a cached handle would keep serving the
    // unlinked inode after a same-name recreate.
    engine::evict_database(name).await;
    if sess
        .turso
        .as_ref()
        .map(|c| matches!(c.try_lock(), Ok(g) if g.db == name))
        == Some(true)
    {
        sess.turso = None;
    }
    if file_present {
        let base = engine::db_path(&shared.conf, name);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let mut p = base.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(p));
        }
    }
    Ok(ExecOutcome::Ok)
}

/// Dispatch arm for the exec layer (engine-enabled builds only; the
/// featureless arm in `exec::dispatch` answers 1235).
pub async fn run(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: &Statement,
) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::CreateDatabase {
            name,
            if_not_exists,
        } => create_database(shared, name, *if_not_exists).await,
        Statement::DropDatabase { name, if_exists } => {
            drop_database(shared, sess, name, *if_exists).await
        }
        _ => unreachable!("exec::dispatch maps only database statements here"),
    }
}
