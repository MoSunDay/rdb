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
        /// Primary-key columns in declaration order: one element for
        /// the classic single-column pk, more for `PRIMARY KEY(a,b)`.
        pk: Vec<String>,
        engine: crate::sql::storage::schema::Engine,
        /// StarRocks table model lifted by the pre-parser (`None` for
        /// plain MySQL DDL; see `parse::starrocks`).
        starrocks: Option<crate::sql::parse::starrocks::StarRocksModel>,
        /// Inline `KEY idx (col)` / `UNIQUE KEY uq (col)` constraints:
        /// single-column indexes declared with the table. They land as
        /// real index entries right after the schema put (same path as
        /// CREATE INDEX; the fresh table has nothing to backfill).
        indexes: Vec<InlineIndex>,
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
    /// `TRUNCATE [TABLE] t`: DDL semantics (decision point 5 of the
    /// mysql-gap plan) -- rejected inside an open txn like every DDL,
    /// not rollbackable, wipes rows/indexes/columnar segments and
    /// resets AUTO_INCREMENT (implemented as a same-name table-id
    /// swap, see `exec::ddl_alter`).
    TruncateTable {
        name: String,
    },
    /// `RENAME TABLE a TO b` / `ALTER TABLE a RENAME [TO|AS] b`:
    /// catalog-only rename; `table_id` and every physical key stay
    /// put, so data/indexes/auto-increment survive in place.
    RenameTable {
        from: String,
        to: String,
    },
    /// INSERT (and its MySQL conflict forms REPLACE / ON DUPLICATE KEY
    /// UPDATE): `columns` is the explicit column list (empty =
    /// positional over every table column), `source` the row source,
    /// `conflict` the conflict semantics.
    Insert {
        table: String,
        columns: Vec<String>,
        source: InsertSource,
        conflict: ConflictAction,
    },
    Select(Query),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
        order_by: Vec<OrderKey>,
        limit: Option<LimitValue>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
        order_by: Vec<OrderKey>,
        limit: Option<LimitValue>,
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
    /// SHOW CREATE TABLE <table>: canonical MySQL-style DDL rendered
    /// from the stored schema (re-executable round trip).
    ShowCreateTable(String),
    /// SHOW DATABASES: the single-database model lists the default db
    /// (plus the USE target when the session picked one).
    ShowDatabases,
    /// `SHOW [GLOBAL|SESSION] VARIABLES [LIKE 'pat']` (the scope
    /// keywords are accepted and ignored -- variables are static).
    ShowVariables {
        like: Option<String>,
    },
    /// `SHOW [GLOBAL|SESSION] STATUS [LIKE 'pat']` over the minimal
    /// honest status subset (no fabricated load counters).
    ShowStatus {
        like: Option<String>,
    },
    /// SET ...: accepted and ignored (no session variables in v1).
    SetIgnored,
    /// A compound query: optional CTEs, a set-operation body
    /// (UNION [ALL]) or a single SELECT, plus trailing ORDER BY /
    /// LIMIT that apply to the whole compound.
    SelectCompound(Box<CompoundQuery>),
}

/// One inline CREATE TABLE index constraint (`KEY` / `UNIQUE KEY`).
#[derive(Debug, Clone, PartialEq)]
pub struct InlineIndex {
    pub name: String,
    pub column: String,
    pub unique: bool,
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

/// Row source of an INSERT. `INSERT ... SET c = e` normalizes to one
/// [`InsertSource::Values`] row at translate time (the SET form is
/// exactly a named single-row VALUES list).
#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    /// `VALUES (e, ...), (e, ...)`: per-row expressions, evaluated
    /// with no row context (column references reject).
    Values(Vec<Vec<Expr>>),
    /// `INSERT ... SELECT ...`: a compound query materialized to
    /// completion BEFORE any write of the statement lands, so
    /// `INSERT INTO t SELECT ... FROM t` reads the pre-statement
    /// snapshot.
    Select(Box<CompoundQuery>),
}

/// Conflict semantics of an INSERT against existing rows.
#[derive(Debug, Clone, PartialEq)]
pub enum ConflictAction {
    /// Plain INSERT: a pk collision keeps the historical silent
    /// last-writer-wins upsert (the StarRocks PRIMARY KEY model; see
    /// `exec::write`), a unique-index collision still rejects 1062.
    /// ODKU/REPLACE below are the EXPLICIT conflict paths.
    Error,
    /// `ON DUPLICATE KEY UPDATE assignments`: the first conflicting
    /// live row (pk first, then unique indexes in column order) takes
    /// the UPDATE branch; `VALUES(col)` in an assignment (IR:
    /// [`Expr::InsertValues`]) reads the incoming row's column.
    OnDuplicate(Vec<(String, Expr)>),
    /// `REPLACE INTO`: delete every conflicting row (pk + unique
    /// hits), then insert the incoming row.
    Replace,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub items: Vec<SelectItem>,
    pub from: TableRef,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<LimitValue>,
    pub offset: LimitValue,
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

/// LIMIT / OFFSET value: a numeric literal, or a `?` placeholder that
/// survives `bind_placeholders` (which turns it into a bound literal)
/// and is coerced to u64 once, at execution time -- MySQL
/// prepared-statement `LIMIT ?` / `OFFSET ?`.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitValue {
    Const(u64),
    /// Boxed: `Statement::Select(Query)` must not grow past the
    /// variant-size lint threshold (clippy -D warnings in CI).
    Param(Box<Expr>),
}

impl LimitValue {
    /// LIMIT/OFFSET absent: the zero placeholder every query without
    /// an OFFSET clause carries.
    pub fn zero() -> LimitValue {
        LimitValue::Const(0)
    }
}

impl ConflictAction {
    /// The assignment list of `ON DUPLICATE KEY UPDATE` (None for the
    /// plain and REPLACE forms).
    pub fn assignments(&self) -> Option<&[(String, Expr)]> {
        match self {
            ConflictAction::OnDuplicate(a) => Some(a),
            ConflictAction::Error | ConflictAction::Replace => None,
        }
    }
}

/// Top-level query expression: CTE scope, set-operation body, and
/// the trailing ORDER BY / LIMIT / OFFSET owned by the outermost
/// query (inner operands carry none).
#[derive(Debug, Clone, PartialEq)]
pub struct CompoundQuery {
    /// `WITH name AS (...)` definitions visible to the body (and to
    /// later definitions); `RECURSIVE` is rejected at parse time.
    pub ctes: Vec<Cte>,
    pub body: QueryBody,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<LimitValue>,
    pub offset: LimitValue,
}

/// Set-operation tree over plain SELECTs: UNION / INTERSECT / EXCEPT,
/// each `[ALL]` (plain = DISTINCT). MySQL has no INTERSECT precedence
/// rule: mixed chains evaluate exactly as the parser nests them
/// (sqlparser folds same-precedence chains left-to-right).
#[derive(Debug, Clone, PartialEq)]
pub enum QueryBody {
    Select(Box<Query>),
    /// A parenthesized full query `(SELECT ... ORDER BY ... LIMIT
    /// ...)`: evaluated as its own compound, own trailing clauses.
    Nested(Box<CompoundQuery>),
    SetOp {
        op: SetOp,
        left: Box<QueryBody>,
        right: Box<QueryBody>,
        /// `... ALL` keeps duplicates; plain / `DISTINCT` dedups.
        all: bool,
    },
}

/// One set operator. `MINUS` stays rejected at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

impl std::fmt::Display for SetOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SetOp::Union => "UNION",
            SetOp::Intersect => "INTERSECT",
            SetOp::Except => "EXCEPT",
        })
    }
}

/// One non-recursive common table expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    /// Optional column alias list (`WITH x (a, b) AS ...`); applied
    /// positionally to the CTE output columns.
    pub column_aliases: Vec<String>,
    pub query: Box<CompoundQuery>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Table {
        name: String,
        alias: Option<String>,
    },
    /// `SELECT 1` / `SELECT (subquery)`: no FROM clause. Materializes
    /// as exactly one empty row with an empty resolution scope.
    NoTable,
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
    /// Derived table: `FROM (SELECT ...) alias`. Materialized once
    /// per query into an in-memory relation.
    Derived {
        query: Box<CompoundQuery>,
        alias: String,
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
    /// GROUP_CONCAT: `sep` rides on the [`Expr::Agg`] node (a call
    /// property, not a function kind).
    GroupConcat,
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
    /// Scalar subquery `(SELECT ...)`: single column, at most one
    /// row (empty -> NULL). Pre-materialized before row evaluation.
    Subquery(Box<CompoundQuery>),
    /// `expr [NOT] IN (SELECT ...)`: pre-materialized into an
    /// `InList` before row evaluation (uncorrelated only).
    InSubquery {
        expr: Box<Expr>,
        query: Box<CompoundQuery>,
        negated: bool,
    },
    /// `[NOT] EXISTS (SELECT ...)`: true iff the subquery yields at
    /// least one row. Uncorrelated forms fold to a literal at
    /// rewrite time; correlated ones become [`Expr::Correlated`].
    Exists {
        query: Box<CompoundQuery>,
        negated: bool,
    },
    /// A correlated subquery's result, pre-computed per distinct
    /// outer-row binding by `exec::correlated::bind` (pure data, no
    /// storage access at evaluation time). `keys` are outer-scope
    /// expressions; evaluating them against the current row picks the
    /// matching `cases` entry (entries were computed for every key
    /// tuple the outer rows produce, so a miss falls back to the
    /// SQL default: NULL scalar / false EXISTS / empty IN set).
    Correlated {
        kind: CorrelatedKind,
        keys: Vec<Expr>,
        cases: Vec<(Vec<Value>, CorrelatedOut)>,
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
    /// `CASE [operand] WHEN ... THEN ... [ELSE ...] END` (both forms).
    /// `operand` present = simple CASE (WHEN values compare `=`);
    /// absent = searched CASE (WHEN conditions must be true).
    Case {
        operand: Option<Box<Expr>>,
        /// (WHEN condition-or-value, THEN result) pairs, in order.
        branches: Vec<(Expr, Expr)>,
        else_expr: Option<Box<Expr>>,
    },
    /// `CAST(expr AS spec)` / `CONVERT(expr, spec)`. `spec` is the
    /// narrow MySQL cast target set the storage layer can express.
    Cast {
        expr: Box<Expr>,
        to: CastSpec,
    },
    /// `expr [NOT] REGEXP|RLIKE pattern` (byte-oriented regex match).
    Regexp {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Agg {
        func: AggFunc,
        arg: Option<Box<Expr>>,
        distinct: bool,
        /// GROUP_CONCAT separator (default `,` when None); other
        /// aggregates ignore it.
        sep: Option<String>,
    },
    Func {
        name: String,
        args: Vec<Expr>,
    },
    /// `VALUES(col)` inside an ON DUPLICATE KEY UPDATE assignment: the
    /// INCOMING row's value of `col`. Only `parse::translate_dml`
    /// produces it (a `VALUES()` call anywhere else is a parse error)
    /// and only the upsert path evaluates it -- by substituting the
    /// incoming row's literal before generic evaluation.
    InsertValues(String),
}

/// Which subquery flavor a [`Expr::Correlated`] node replaced; the
/// kind decides how a binding's pre-computed output maps to a value.
#[derive(Debug, Clone, PartialEq)]
pub enum CorrelatedKind {
    /// `(SELECT ...)`: single column, at most one row per binding
    /// (empty -> NULL, more -> MySQL 1242 style error at bind time).
    Scalar,
    /// `[NOT] EXISTS (...)`: rows-nonempty test.
    Exists { negated: bool },
    /// `lhs [NOT] IN (SELECT ...)`: `lhs` evaluates in the OUTER
    /// scope per row; the binding contributes the member set.
    In { lhs: Box<Expr>, negated: bool },
}

/// One binding's subquery output.
#[derive(Debug, Clone, PartialEq)]
pub enum CorrelatedOut {
    /// Scalar result (exactly the one cell).
    Scalar(Value),
    /// First-column values of the subquery rows (IN / EXISTS).
    Rows(Vec<Value>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    /// `<=>`: NULL-safe equality (NULL <=> NULL is TRUE).
    NullSafeEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    /// `XOR` keyword: three-valued logical exclusion.
    LogicalXor,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    /// `&`, `|`, `^`: 64-bit unsigned semantics on Int (u64 wrap).
    BitAnd,
    BitOr,
    BitXor,
    /// `<<` / `>>`: u64 shifts; a shift >= 64 yields 0 (MySQL).
    Shl,
    Shr,
}

/// Target of `CAST(x AS spec)` / `CONVERT(x, spec)`: the MySQL cast
/// types the storage layer can express (SIGNED/UNSIGNED integers,
/// CHAR(n) strings, DECIMAL(p,s) exact fixed-point).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastSpec {
    Signed,
    Unsigned,
    /// `CHAR(n)`: n is the optional length in characters (truncate).
    Char(Option<u32>),
    Decimal {
        precision: u8,
        scale: u8,
    },
}

impl Statement {
    /// Monitor label for `rdb_sql_query_latency`.
    pub fn metric_kind(&self) -> &'static str {
        match self {
            Statement::CreateTable { .. }
            | Statement::DropTable { .. }
            | Statement::CreateIndex { .. }
            | Statement::DropIndex { .. }
            | Statement::TruncateTable { .. }
            | Statement::RenameTable { .. } => "ddl",
            Statement::Select(_) => "select",
            Statement::Insert { .. } => "insert",
            Statement::Update { .. } => "update",
            Statement::Delete { .. } => "delete",
            Statement::Explain(_) => "explain",
            _ => "other",
        }
    }
}
