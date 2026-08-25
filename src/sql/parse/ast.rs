//! Internal SQL IR: a narrow, executor-friendly projection of the
//! sqlparser AST. Only what the v1 engine can actually run survives
//! translation; anything wider fails with an explicit unsupported error.

use crate::sql::storage::schema::Value;

/// One parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        if_not_exists: bool,
        columns: Vec<ColumnSpec>,
        pk: String,
        engine: crate::sql::storage::schema::Engine,
    },
    DropTable {
        name: String,
        if_exists: bool,
    },
    CreateIndex {
        table: String,
        name: String,
        column: String,
        unique: bool,
        if_not_exists: bool,
    },
    DropIndex {
        table: String,
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Expr>>,
    },
    Select(Query),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
        order_by: Vec<OrderKey>,
        limit: Option<u64>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
        order_by: Vec<OrderKey>,
        limit: Option<u64>,
    },
    Explain(Box<Statement>),
    Begin,
    Commit,
    Rollback,
    /// `SAVEPOINT name` (no-op without an open txn; MySQL errors only
    /// on ROLLBACK TO -- same laxness here).
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name`: undo staged writes back to the
    /// savepoint's snapshot of the write set, keep the txn open.
    RollbackTo {
        name: String,
    },
    /// `RELEASE [SAVEPOINT] name`: forget the savepoint (no row undo).
    ReleaseSavepoint {
        name: String,
    },
    /// `SET [SESSION|GLOBAL] TRANSACTION ISOLATION LEVEL ...`: accepted
    /// and mapped onto the engine's snapshot isolation, which already
    /// provides REPEATABLE READ (the MySQL default) semantics.
    SetIsolation {
        level: String,
    },
    Use(String),
    ShowTables,
    ShowColumns(String),
    /// SHOW INDEX FROM <table> (sqlparser parses it as ShowVariable).
    ShowIndexes(String),
    /// SET ...: accepted and ignored (no session variables in v1).
    SetIgnored,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSpec {
    pub name: String,
    pub sql_type: crate::sql::storage::schema::SqlType,
    pub nullable: bool,
    /// MySQL `AUTO_INCREMENT`: server-side value allocation on INSERT
    /// when the column is omitted (or NULL/0 is supplied).
    pub auto_increment: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub items: Vec<SelectItem>,
    pub from: TableRef,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<u64>,
    pub offset: u64,
    pub distinct: bool,
    /// Trailing locking clause (`FOR UPDATE` / `FOR SHARE`): metadata
    /// only -- the result shape is unchanged; the executor takes row
    /// latches on the matched pks (see `tx::latch`).
    pub lock: Option<LockRead>,
}

/// Locking-read mode parsed off `FOR UPDATE` / `FOR SHARE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockRead {
    ForUpdate,
    ForShare,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: String,
        alias: Option<String>,
    },
    Join {
        left: Box<TableRef>,
        right: Box<TableRef>,
        kind: JoinKind,
        /// Join condition; `None` for cross joins.
        on: Option<Expr>,
        /// `JOIN ... USING (cols)`: equality on each same-named column
        /// of both sides, resolved at materialization time (the AST
        /// layer knows no schemas).
        using: Vec<String>,
    },
}

/// The join flavors the executor understands (OUTER sides null-extend).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    /// Explicit `CROSS JOIN`: same evaluation as `Inner` with no ON.
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    pub expr: Expr,
    pub asc: bool,
}

/// Aggregate functions (M2 eval, but parsed from M1 so errors are early).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Col {
        table: Option<String>,
        name: String,
    },
    Lit(Value),
    /// `?` placeholder; bound to a value before execution.
    Placeholder,
    BinaryOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    Neg(Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Agg {
        func: AggFunc,
        arg: Option<Box<Expr>>,
        distinct: bool,
    },
    Func {
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

impl Statement {
    /// Monitor label for `rdb_sql_query_latency`.
    pub fn metric_kind(&self) -> &'static str {
        match self {
            Statement::CreateTable { .. }
            | Statement::DropTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::DropIndex { .. } => "ddl",
            Statement::Select(_) => "select",
            Statement::Insert { .. } => "insert",
            Statement::Update { .. } => "update",
            Statement::Delete { .. } => "delete",
            Statement::Explain(_) => "explain",
            _ => "other",
        }
    }
}
