//! Per-statement routing between the host (rocks) engine and the
//! embedded turso engine. The routing key is the session's logical
//! database: a `turso_*` name on an engine-enabled node rides turso,
//! everything else falls through to the host executor unchanged.

use crate::sql::exec::sequence;
use crate::sql::exec::{ExecOutcome, SqlSession};
use crate::sql::parse::ast::{Query, Statement};
use crate::sql::parse::error::SqlResult;
use crate::sql::storage::schema::Value;
use crate::state::Shared;

use super::conv;
use super::engine::{self, ConnHandle};
use super::meta;
use super::translate;

/// Route one statement. `Ok(None)` = not this engine's business (the
/// caller falls through to the host executor); `Ok(Some)` = executed
/// here; `Err` = engine-bound but rejected (T1 shape) or failed.
pub async fn route(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: &Statement,
) -> SqlResult<Option<ExecOutcome>> {
    if !engine::enabled(&shared.conf) || !engine::is_turso_db(&sess.db) {
        return Ok(None);
    }
    let out = match stmt {
        Statement::Select(q) => select(shared, sess, q).await?,
        // Set operations and CTEs stay whole-engine rejections: the
        // host executor would answer from ITS tables (the library's
        // rows are invisible there), which is silent wrong data.
        Statement::SelectCompound(_) => {
            return Err(engine::turso_unsupported(
                "UNION/INTERSECT/EXCEPT set operations",
            ))
        }
        // CREATE TABLE: the statement list is [table, inline
        // indexes...] in order (the indexes ride CREATE INDEX after
        // the table exists). Any DDL here also drops the cached
        // column metadata of this library (vector write-path checks).
        Statement::CreateTable { .. } => {
            let stmts = translate::render_create_table(stmt)?;
            let conn = pin(shared, sess).await?;
            for sql in &stmts {
                let guard = conn.lock().await;
                engine::run_execute(&guard, sql).await?;
            }
            meta::invalidate_db(&sess.db);
            ExecOutcome::Ok
        }
        Statement::DropTable { name, if_exists } => {
            let sql = translate::render_drop_table(name, *if_exists);
            ddl(shared, sess, &sql).await?;
            meta::invalidate_db(&sess.db);
            ExecOutcome::Ok
        }
        Statement::CreateIndex { .. } => {
            let sql = translate::render_create_index(stmt)?;
            ddl(shared, sess, &sql).await?
        }
        Statement::DropIndex { .. } => {
            let sql = translate::render_drop_index(stmt)?;
            ddl(shared, sess, &sql).await?
        }
        // TRUNCATE = engine DELETE with MySQL's DDL outcome shape
        // (zero affected rows); the AUTO_INCREMENT watermark does not
        // reset (translate::render_truncate's deviation note).
        Statement::TruncateTable { name } => {
            let sql = translate::render_truncate(name);
            let conn = pin(shared, sess).await?;
            let guard = conn.lock().await;
            engine::run_execute(&guard, &sql).await?;
            drop(guard);
            ExecOutcome::Ok
        }
        Statement::Insert { table, .. } => {
            // Vector(n) columns validate app-side before the engine
            // sees the write (D6): the renderer needs the table's
            // column metadata first.
            let conn = pin(shared, sess).await?;
            let guard = conn.lock().await;
            let cols = meta::table_columns(&guard, table).await?;
            let sql = translate::render_insert(stmt, Some(&cols))?;
            let n = engine::run_execute(&guard, &sql).await?;
            // MySQL's OK packet carries the connection's
            // last-insert id: the pinned connection's own rowid
            // watermark (first-vs-last of a multi-row insert and
            // explicit-id no-op drifts are ledger entries).
            let id = guard.conn.last_insert_rowid();
            drop(guard);
            if id > 0 {
                sess.last_insert_id = id;
                sequence::note_last_insert_id(id);
            }
            ExecOutcome::Affected(n)
        }
        // UPDATE/DELETE: affected = the engine's changed-rows return.
        Statement::Update { table, .. } => {
            let conn = pin(shared, sess).await?;
            let guard = conn.lock().await;
            let cols = meta::table_columns(&guard, table).await?;
            let sql = translate::render_update(stmt, Some(&cols))?;
            let n = engine::run_execute(&guard, &sql).await?;
            drop(guard);
            ExecOutcome::Affected(n)
        }
        Statement::Delete { .. } => {
            let sql = translate::render_delete(stmt)?;
            write(shared, sess, &sql).await?
        }
        // The turso side of the metadata SHOWs (read from the
        // library's own sqlite_master, never the host catalog).
        Statement::ShowTables => {
            let db = sess.db.clone();
            let conn = pin(shared, sess).await?;
            let guard = conn.lock().await;
            let out = meta::show_tables(&db, &guard).await?;
            drop(guard);
            out
        }
        Statement::ShowColumns(table) => {
            let conn = pin(shared, sess).await?;
            let guard = conn.lock().await;
            let out = meta::show_columns(&guard, table).await?;
            drop(guard);
            out
        }
        Statement::ShowCreateTable(table) => {
            let conn = pin(shared, sess).await?;
            let guard = conn.lock().await;
            let out = meta::show_create_table(&guard, table).await?;
            drop(guard);
            out
        }
        // Engine-bound kinds that stay unimplemented: loud 1235, never
        // a silent fallthrough to the wrong engine's tables. CREATE
        // VECTOR INDEX carries its own verdict: the engine answers
        // exact KNN over the distance functions, ANN is the host
        // (rocks) engine's index family.
        Statement::CreateVectorIndex { .. } => {
            return Err(crate::sql::parse::error::SqlError::unsupported(
                "CREATE VECTOR INDEX on turso engine \
                 (exact KNN only; ANN is rocks-engine only; see COMPAT.sql.md)",
            ));
        }
        Statement::RenameTable { .. } | Statement::ShowIndexes(_) => {
            return Err(engine::turso_unsupported(kind_of(stmt)));
        }
        // Explicit transactions ride the session's pinned connection
        // as string passthrough (D7): plain BEGIN (DEFERRED -- BEGIN
        // CONCURRENT needs the mvcc journal mode and is never
        // forwarded), COMMIT, ROLLBACK. MySQL's BEGIN-implies-commit
        // semantics are emulated app-side below. The host arms never
        // run for turso sessions: `sess.txn` stays None and the
        // engine connection IS the transaction state.
        Statement::Begin => txn_begin(shared, sess).await?,
        Statement::Commit => txn_commit(shared, sess).await?,
        Statement::Rollback => txn_rollback(shared, sess).await?,
        // USE / SHOW DATABASES / SET / ... stay host-side: they
        // address session or cluster state, not engine storage.
        _ => return Ok(None),
    };
    Ok(Some(out))
}

/// The statement-kind label the 1235 rejections carry.
fn kind_of(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::Insert { .. } => "INSERT",
        Statement::Update { .. } => "UPDATE",
        Statement::Delete { .. } => "DELETE",
        Statement::CreateTable { .. } => "CREATE TABLE",
        Statement::DropTable { .. } => "DROP TABLE",
        Statement::CreateIndex { .. } => "CREATE INDEX",
        Statement::CreateVectorIndex { .. } => "CREATE VECTOR INDEX",
        Statement::DropIndex { .. } => "DROP INDEX",
        Statement::TruncateTable { .. } => "TRUNCATE",
        Statement::RenameTable { .. } => "RENAME TABLE",
        Statement::ShowIndexes(_) => "SHOW INDEX",
        _ => "statement",
    }
}

/// Pin (or re-pin) the session's engine connection for the statements
/// below; a shared helper because every executable arm needs one.
async fn pin(
    shared: &Shared,
    sess: &mut SqlSession,
) -> SqlResult<std::sync::Arc<tokio::sync::Mutex<ConnHandle>>> {
    let db = sess.db.clone();
    engine::session_conn(&shared.conf, sess, &db).await
}

/// BEGIN with MySQL semantics: an open transaction is implicitly
/// committed first (turso's own nested-BEGIN error never surfaces),
/// then a plain DEFERRED transaction starts on the pinned connection.
async fn txn_begin(shared: &Shared, sess: &mut SqlSession) -> SqlResult<ExecOutcome> {
    let conn = pin(shared, sess).await?;
    let guard = conn.lock().await;
    if engine::in_txn(&guard).await? {
        engine::run_execute(&guard, "COMMIT").await?;
    }
    engine::run_execute(&guard, "BEGIN").await?;
    drop(guard);
    Ok(ExecOutcome::Ok)
}

/// COMMIT outside a transaction is MySQL's no-op OK; inside one the
/// pinned connection commits (any Busy after the 5s busy timeout
/// maps to 1205 via map_error).
async fn txn_commit(shared: &Shared, sess: &mut SqlSession) -> SqlResult<ExecOutcome> {
    let conn = pin(shared, sess).await?;
    let guard = conn.lock().await;
    if engine::in_txn(&guard).await? {
        engine::run_execute(&guard, "COMMIT").await?;
    }
    drop(guard);
    Ok(ExecOutcome::Ok)
}

/// ROLLBACK outside a transaction is a no-op OK (MySQL shape); inside
/// one the pinned connection rolls back, errors mapped verbatim.
async fn txn_rollback(shared: &Shared, sess: &mut SqlSession) -> SqlResult<ExecOutcome> {
    let conn = pin(shared, sess).await?;
    let guard = conn.lock().await;
    if engine::in_txn(&guard).await? {
        engine::run_execute(&guard, "ROLLBACK").await?;
    }
    drop(guard);
    Ok(ExecOutcome::Ok)
}

/// One write-shaped statement: run it, report the affected count.
async fn write(shared: &Shared, sess: &mut SqlSession, sql: &str) -> SqlResult<ExecOutcome> {
    let conn = pin(shared, sess).await?;
    let guard = conn.lock().await;
    let n = engine::run_execute(&guard, sql).await?;
    drop(guard);
    Ok(ExecOutcome::Affected(n))
}

async fn ddl(shared: &Shared, sess: &mut SqlSession, sql: &str) -> SqlResult<ExecOutcome> {
    write(shared, sess, sql).await?;
    Ok(ExecOutcome::Ok)
}

async fn select(shared: &Shared, sess: &mut SqlSession, q: &Query) -> SqlResult<ExecOutcome> {
    let sql = translate::render_select(q)?;
    let conn = pin(shared, sess).await?;
    let guard = conn.lock().await;
    let (cols, raw) = engine::run_query(&guard, &sql).await?;
    drop(guard);
    // Vector-typed table columns decode their blob payloads back into
    // Value::Vector (the MySQL layer ships '[e,e,...]' text); every
    // other cell rides the plain conversion.
    let rows: Vec<Vec<Value>> = raw
        .into_iter()
        .map(|r| {
            r.into_iter()
                .zip(cols.iter())
                .map(|(v, (_, decl))| conv::value_from_typed(v, decl.as_deref()))
                .collect()
        })
        .collect();
    let columns = cols
        .iter()
        .enumerate()
        .map(|(i, (name, decl))| {
            conv::colmeta_from(name, decl.as_deref(), rows.iter().find_map(|r| r.get(i)))
        })
        .collect();
    Ok(ExecOutcome::Rows { columns, rows })
}
