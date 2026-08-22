//! MySQL-protocol frontend: the wire layer that makes the SQL engine
//! speak the MySQL client/server protocol.
//!
//! This front is PROTOCOL-ONLY -- it owns no SQL semantics. Parsing lives
//! in [`crate::sql::parse`], execution in [`crate::sql::exec`]; the front
//! translates protocol events into `parse_statement`/`execute` calls and
//! encodes the resulting `ExecOutcome` back onto the wire.
//!
//! Connection lifecycle ([`serve`]):
//! 1. accept -> spawn one task per connection (resp::serve pattern);
//! 2. `AsyncMysqlIntermediary::run_with_options` drives the packet loop:
//!    handshake -> auth -> command dispatch until the client quits;
//! 3. connection errors are logged and dropped; the listener survives.
//!
//! Auth ([`auth`]): only `mysql_native_password` is offered and verified
//! -- the client's scramble token is recomputed from the configured
//! `mysql_user`/`mysql_password` (`empty user means "root"`, empty
//! password means token-less logins). The handshake salt is per
//! connection so a captured token cannot be replayed against a later one.
//!
//! Prepared statement flow ([`shim`]): PREPARE parses once and stores the
//! `Statement` under a per-connection id (parameters advertised as
//! untyped `?` columns; output columns are not known until execute, so
//! the prepare reply carries an empty column list -- the EXECUTE response
//! supplies the real one). EXECUTE decodes `ParamValue`s into engine
//! values, `bind_placeholders` substitutes them, then execution proceeds
//! exactly like a text query. CLOSE drops the mapping.
//!
//! `USE <db>` is routed by opensrv to `on_init` (not parsed as a query)
//! and only records the session's database name. `BEGIN`/`COMMIT`/
//! `ROLLBACK` reach the executor and get its not-supported error (M1 is
//! autocommit-only).

pub mod auth;
pub mod conv;
pub mod serve;
pub mod shim;
pub mod vars;

pub use serve::{bind, serve};
