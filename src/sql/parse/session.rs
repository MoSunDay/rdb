//! Session-variable statement policy for real-client handshakes.
//!
//! MySQL-wire clients (mycli, pymysql, JDBC, the mysql CLI) all send a
//! short burst of `SET ...` statements right after connecting. The engine
//! honors the ones that only *describe* the connection (charset) or restate
//! its only real mode (`autocommit = 1`), and loudly rejects the ones that
//! would request semantics it does not implement (`autocommit = 0`,
//! isolation levels other than the accepted `SET TRANSACTION` forms,
//! time zones).

use super::ast::Statement;
use super::error::{SqlError, SqlResult};

/// `SET autocommit = 1` spellings accepted as the engine's real mode.
fn autocommit_on(values: &[sqlparser::ast::Expr]) -> bool {
    match values {
        [sqlparser::ast::Expr::Value(v)] => match &v.value {
            sqlparser::ast::Value::Number(n, _) => n == "1",
            sqlparser::ast::Value::Boolean(b) => *b,
            _ => false,
        },
        _ => false,
    }
}

/// Decide one session-variable assignment.
///
/// Accepted (no-op): charset declarations (`names`/`charset`, the engine
/// is utf-8 end-to-end) and `autocommit = 1` (every statement outside an
/// explicit BEGIN commits — this already IS the behavior the client asks
/// for). Everything that would change execution semantics is rejected so
/// a client never silently runs under assumptions we do not honor.
pub(crate) fn reject_session_var(
    variable: &sqlparser::ast::ObjectName,
    values: &[sqlparser::ast::Expr],
    set: &sqlparser::ast::Set,
) -> SqlResult<Statement> {
    let name = variable.to_string().to_ascii_lowercase();
    let on = name.contains("autocommit") && autocommit_on(values);
    let sensitive = ["isolation", "session", "time_zone"]
        .iter()
        .any(|k| name.contains(k));
    if on || (!sensitive && !name.contains("autocommit")) {
        Ok(Statement::SetIgnored)
    } else {
        Err(SqlError::unsupported(format!(
            "{set} (session/transaction settings)"
        )))
    }
}
