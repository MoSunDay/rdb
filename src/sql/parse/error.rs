//! SQL error type and its MySQL error-code mapping.

use opensrv_mysql::ErrorKind;

/// Errors surfaced by parse/plan/execute; `code` picks the MySQL error the
/// client sees, `msg` is the human-readable detail.
#[derive(Debug, Clone, PartialEq)]
pub struct SqlError {
    pub code: ErrorCode,
    pub msg: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Parse,
    NoSuchTable,
    TableExists,
    BadField,
    DupEntry,
    BadNull,
    WrongValueCount,
    NotSupported,
    AccessDenied,
    /// Another transaction committed a newer version of a key this
    /// transaction also wrote (first committer wins).
    WriteConflict,
    /// DDL attempted while an explicit transaction is open.
    TxnDdl,
    /// A scatter-gather read could not reach a cluster node. Fails the
    /// whole query (v1 never serves partial results); HA failover of
    /// SQL reads is future work.
    NodeUnreachable,
    Unknown,
}

impl SqlError {
    pub fn new(code: ErrorCode, msg: impl Into<String>) -> SqlError {
        SqlError {
            code,
            msg: msg.into(),
        }
    }

    pub fn parse(msg: impl Into<String>) -> SqlError {
        Self::new(ErrorCode::Parse, msg)
    }

    pub fn unsupported(msg: impl Into<String>) -> SqlError {
        Self::new(
            ErrorCode::NotSupported,
            format!("not supported in this version: {}", msg.into()),
        )
    }

    pub fn no_such_table(name: &str) -> SqlError {
        Self::new(
            ErrorCode::NoSuchTable,
            format!("table '{name}' doesn't exist"),
        )
    }

    pub fn kind(&self) -> ErrorKind {
        match self.code {
            ErrorCode::Parse => ErrorKind::ER_PARSE_ERROR,
            ErrorCode::NoSuchTable => ErrorKind::ER_NO_SUCH_TABLE,
            ErrorCode::TableExists => ErrorKind::ER_TABLE_EXISTS_ERROR,
            ErrorCode::BadField => ErrorKind::ER_BAD_FIELD_ERROR,
            ErrorCode::DupEntry => ErrorKind::ER_DUP_ENTRY,
            ErrorCode::BadNull => ErrorKind::ER_BAD_NULL_ERROR,
            ErrorCode::WrongValueCount => ErrorKind::ER_WRONG_VALUE_COUNT_ON_ROW,
            ErrorCode::NotSupported => ErrorKind::ER_NOT_SUPPORTED_YET,
            ErrorCode::AccessDenied => ErrorKind::ER_ACCESS_DENIED_ERROR,
            // 1027 (ER_FILE_USED): reads must not silently degrade to
            // partial results when a band owner cannot be reached.
            ErrorCode::NodeUnreachable => ErrorKind::ER_FILE_USED,
            // 1213: the MySQL serialization-failure error clients retry on.
            ErrorCode::WriteConflict => ErrorKind::ER_LOCK_DEADLOCK,
            ErrorCode::TxnDdl => ErrorKind::ER_NOT_SUPPORTED_YET,
            ErrorCode::Unknown => ErrorKind::ER_UNKNOWN_ERROR,
        }
    }
}

impl From<String> for SqlError {
    fn from(storage_err: String) -> Self {
        SqlError::new(ErrorCode::Unknown, storage_err)
    }
}

impl std::fmt::Display for SqlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for SqlError {}

pub type SqlResult<T> = Result<T, SqlError>;
