//! The per-connection MySQL shim: opensrv callbacks -> engine calls.
//!
//! This layer is deliberately THIN: it translates protocol events
//! (handshake auth, COM_QUERY, COM_PREPARE/EXECUTE/CLOSE, COM_INIT_DB)
//! into `parse_statement` / `execute` calls and writes the resulting
//! [`ExecOutcome`] back. Every SQL semantic lives in `sql::exec`; nothing
//! here inspects statement contents beyond placeholder bookkeeping.

use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use opensrv_mysql::{
    AsyncMysqlShim, ErrorKind, InitWriter, IntermediaryOptions, OkResponse, ParamParser,
    QueryResultWriter, StatementMetaWriter,
};
use tokio::io::AsyncWrite;
use tokio::sync::Mutex;

use crate::sql::exec::{self, ExecOutcome, SqlSession};
use crate::sql::front::{auth, conv, vars};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::SqlResult;
use crate::sql::parse::{bind_placeholders, parse_statement, placeholder_count};
use crate::state::Shared;

/// Auth plugin this frontend offers (the only one it can verify).
pub const AUTH_PLUGIN: &str = "mysql_native_password";

/// Server version advertised in the handshake and `@@version`.
pub const SERVER_VERSION: &str = "8.0.32-rdb";

/// Per-connection protocol state. Generic in the write half only so the
/// `AsyncMysqlShim<W>` impl can be written once; `PhantomData<W>` carries
/// no runtime state.
pub struct SqlShim<W> {
    shared: Arc<Shared>,
    user: String,
    password: String,
    /// USE target and the open explicit transaction. Behind an Arc so
    /// the connection runner can still release the txn snapshot after
    /// the packet loop ends (the intermediary consumes the shim).
    sess: Arc<Mutex<SqlSession>>,
    /// Client-assigned id -> parsed-but-unbound statement.
    prepared: HashMap<u32, Statement>,
    next_id: u32,
    /// Per-connection handshake scramble.
    salt: [u8; auth::SCRAMBLE_LEN],
    /// Per-connection id shown to the client.
    id: u32,
    _ph: PhantomData<W>,
}

/// Build a shim for one connection. `seed` drives the handshake salt and
/// connection id (see [`auth::salt_from_seed`]).
pub fn new_shim<W>(shared: Arc<Shared>, user: String, password: String, seed: u64) -> SqlShim<W> {
    SqlShim {
        shared,
        user,
        password,
        sess: Arc::new(Mutex::new(SqlSession::default())),
        prepared: HashMap::new(),
        next_id: 1,
        salt: auth::salt_from_seed(seed),
        id: (seed as u32) | 1,
        _ph: PhantomData,
    }
}

impl<W> SqlShim<W> {
    /// Handle to the session outliving the packet loop (the connection
    /// runner uses it for disconnect cleanup).
    pub fn session_handle(&self) -> Arc<Mutex<SqlSession>> {
        Arc::clone(&self.sess)
    }
}

/// Intermediary options: `USE` is routed to `on_init` (not parsed as a
/// query), and connections without a database name are accepted -- the
/// engine is single-(default-)database, `sess.db` starts empty.
pub fn intermediary_options() -> IntermediaryOptions {
    IntermediaryOptions {
        process_use_statement_on_query: false,
        reject_connection_on_dbname_absence: false,
    }
}

#[async_trait]
impl<W> AsyncMysqlShim<W> for SqlShim<W>
where
    W: AsyncWrite + Send + Sync + Unpin,
{
    type Error = io::Error;

    fn version(&self) -> String {
        "8.0.32-rdb".to_string()
    }

    fn connect_id(&self) -> u32 {
        self.id
    }

    fn salt(&self) -> [u8; 20] {
        self.salt
    }

    async fn authenticate(
        &self,
        plugin: &str,
        username: &[u8],
        salt: &[u8],
        auth_data: &[u8],
    ) -> bool {
        plugin == AUTH_PLUGIN
            && username == self.user.as_bytes()
            && auth::native_password_matches(salt, &self.password, auth_data)
    }

    async fn on_query<'a>(
        &'a mut self,
        query: &'a str,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let out = {
            let mut sess = self.sess.lock().await;
            run_statement(&self.shared, &mut sess, query).await
        };
        write_outcome(results, out).await
    }

    async fn on_prepare<'a>(
        &'a mut self,
        query: &'a str,
        info: StatementMetaWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let stmt = match parse_statement(query.trim()) {
            Ok(s) => s,
            Err(e) => {
                info.error(e.kind(), e.msg.as_bytes()).await?;
                return Ok(());
            }
        };
        // Output columns are unknown until EXECUTE (they depend on the
        // projection); the protocol allows an empty prepare-time list,
        // the execute response carries the real column set.
        let params = conv::placeholder_columns(placeholder_count(&stmt));
        let id = self.alloc_stmt_id();
        self.prepared.insert(id, stmt);
        info.reply(
            id,
            params.iter(),
            std::iter::empty::<&opensrv_mysql::Column>(),
        )
        .await?;
        Ok(())
    }

    async fn on_execute<'a>(
        &'a mut self,
        id: u32,
        params: ParamParser<'a>,
        results: QueryResultWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        let Some(stmt) = self.prepared.get(&id).cloned() else {
            results
                .error(
                    ErrorKind::ER_UNKNOWN_STMT_HANDLER,
                    format!("unknown prepared statement {id}").as_bytes(),
                )
                .await?;
            return Ok(());
        };
        let mut stmt = stmt;
        let mut values = Vec::new();
        for pv in params {
            match conv::param_to_value(&pv) {
                Ok(v) => values.push(v),
                Err(m) => {
                    results
                        .error(ErrorKind::ER_NOT_SUPPORTED_YET, m.as_bytes())
                        .await?;
                    return Ok(());
                }
            }
        }
        if let Err(e) = bind_placeholders(&mut stmt, &values) {
            results.error(e.kind(), e.msg.as_bytes()).await?;
            return Ok(());
        }
        let out = {
            let mut sess = self.sess.lock().await;
            exec::execute(&self.shared, &mut sess, stmt).await
        };
        write_outcome(results, out).await
    }

    async fn on_close(&mut self, stmt: u32) {
        self.prepared.remove(&stmt);
    }

    async fn on_init<'a>(
        &'a mut self,
        db: &'a str,
        w: InitWriter<'a, W>,
    ) -> Result<(), Self::Error> {
        self.sess.lock().await.db = db.to_string();
        w.ok().await
    }
}

impl<W> SqlShim<W> {
    fn alloc_stmt_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

/// Parse + execute one query text (shared by on_query's direct path).
/// `SELECT @@...` system-variable probes are answered by the frontend's
/// compatibility table before the engine sees them (see `vars.rs`).
async fn run_statement(
    shared: &Shared,
    sess: &mut SqlSession,
    query: &str,
) -> SqlResult<ExecOutcome> {
    let trimmed = query.trim().trim_end_matches(';').trim();
    if let Some(names) = vars::parse_sysvar_query(trimmed) {
        return vars::sysvar_outcome(&names, SERVER_VERSION);
    }
    let stmt = parse_statement(trimmed)?;
    exec::execute(shared, sess, stmt).await
}

/// Encode one executor outcome (or error) as a MySQL response. Shared by
/// the text and binary (prepared) paths.
async fn write_outcome<W>(
    results: QueryResultWriter<'_, W>,
    out: SqlResult<ExecOutcome>,
) -> io::Result<()>
where
    W: AsyncWrite + Send + Unpin,
{
    match out {
        Ok(ExecOutcome::Ok) => results.completed(OkResponse::default()).await,
        Ok(ExecOutcome::Affected(n)) => {
            results
                .completed(OkResponse {
                    affected_rows: n,
                    ..Default::default()
                })
                .await
        }
        Ok(ExecOutcome::Rows { columns, rows }) => {
            let cols = conv::colmetas_to_columns(&columns);
            let mut w = results.start(&cols).await?;
            for row in rows {
                for cell in &row {
                    conv::write_value(&mut w, cell)?;
                }
                w.end_row().await?;
            }
            w.finish().await
        }
        Err(e) => results.error(e.kind(), e.msg.as_bytes()).await,
    }
}
