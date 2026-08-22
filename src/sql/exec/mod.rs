//! Executor entry points.
//!
//! [`execute`] dispatches one parsed statement. M1 semantics: every
//! statement autocommits -- reads run at the oracle's `now()` snapshot,
//! writes allocate a fresh timestamp per batch. M2 layers explicit
//! BEGIN/COMMIT snapshot transactions on top of the same entry point.

pub mod agg;
pub mod ddl;
pub mod expr;
pub mod render;
pub mod scan;
pub mod select;
pub mod show;
pub mod write;

use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::SqlType;
use crate::sql::tx;
use crate::state::Shared;

/// Result column metadata (name + wire type) for row-producing statements.
#[derive(Debug, Clone, PartialEq)]
pub struct ColMeta {
    /// Table qualifier ("" when the column is computed).
    pub table: String,
    pub name: String,
    pub sql_type: SqlType,
}

/// Statement outcome: a rowset (SELECT/SHOW/EXPLAIN), an affected-rows
/// count (INSERT/UPDATE/DELETE), or plain OK (DDL/SET).
#[derive(Debug, Clone, PartialEq)]
pub enum ExecOutcome {
    Rows {
        columns: Vec<ColMeta>,
        rows: Vec<Vec<crate::sql::storage::schema::Value>>,
    },
    Affected(u64),
    Ok,
}

/// Per-connection SQL session state (USE target + the open explicit
/// transaction). One instance lives in the frontend shim per connection.
#[derive(Debug, Default, Clone)]
pub struct SqlSession {
    pub db: String,
    /// Open BEGIN..COMMIT/ROLLBACK transaction, if any: a pinned
    /// snapshot plus the staged write set (see [`crate::sql::tx`]).
    pub txn: Option<crate::sql::tx::Txn>,
}

/// Execute one statement against the shared engine state.
pub async fn execute(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: Statement,
) -> SqlResult<ExecOutcome> {
    let start = std::time::Instant::now();
    let kind = stmt.metric_kind();
    let out = dispatch(shared, sess, stmt).await;
    crate::monitor::observe_sql_latency(
        &shared.monitor,
        kind,
        start.elapsed().as_secs_f64() * 1000.0,
    );
    out
}

async fn dispatch(
    shared: &Shared,
    sess: &mut SqlSession,
    stmt: Statement,
) -> SqlResult<ExecOutcome> {
    // DDL mutates the raft-replicated catalog; refusing it inside an
    // open transaction keeps snapshot reads consistent with the schema
    // they were planned against.
    if sess.txn.is_some()
        && matches!(
            stmt,
            Statement::CreateTable { .. }
                | Statement::DropTable { .. }
                | Statement::CreateIndex { .. }
                | Statement::DropIndex { .. }
        )
    {
        return Err(SqlError::new(
            ErrorCode::TxnDdl,
            "DDL not allowed inside a transaction".to_string(),
        ));
    }
    match stmt {
        Statement::Begin => {
            // MySQL semantics: BEGIN implicitly commits any open txn.
            if let Some(txn) = sess.txn.take() {
                tx::commit(shared, txn).await?;
            }
            sess.txn = Some(tx::begin(&shared.sql_ts));
            Ok(ExecOutcome::Ok)
        }
        Statement::Commit => match sess.txn.take() {
            Some(txn) => {
                tx::commit(shared, txn).await?;
                Ok(ExecOutcome::Ok)
            }
            None => Ok(ExecOutcome::Ok),
        },
        Statement::Rollback => {
            if let Some(txn) = sess.txn.take() {
                tx::rollback(&shared.sql_ts, txn);
            }
            Ok(ExecOutcome::Ok)
        }
        Statement::CreateTable { .. } | Statement::DropTable { .. } => ddl::run(shared, stmt).await,
        Statement::CreateIndex { .. } | Statement::DropIndex { .. } => ddl::run(shared, stmt).await,
        Statement::Insert { .. } => write::insert(shared, sess, stmt).await,
        Statement::Update { .. } => write::update(shared, sess, stmt).await,
        Statement::Delete { .. } => write::delete(shared, sess, stmt).await,
        Statement::Select(q) => {
            let (columns, rows) = select::run(shared, sess, q).await?;
            Ok(ExecOutcome::Rows { columns, rows })
        }
        Statement::Explain(inner) => select::explain(shared, &inner),
        Statement::ShowTables | Statement::ShowColumns(_) | Statement::ShowIndexes(_) => {
            show::run(shared, sess, &stmt)
        }
        Statement::Use(db) => {
            sess.db = db;
            Ok(ExecOutcome::Ok)
        }
        Statement::SetIgnored => Ok(ExecOutcome::Ok),
    }
}
