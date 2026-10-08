//! sqlparser (MySQL dialect) AST -> internal IR.
//!
//! Parse with [`parse_statement`]; anything the v1 engine cannot run is
//! rejected here with an explicit `not supported` / parse error so the
//! executor only ever sees runnable shapes.

use sqlparser::ast::{
    CreateTableOptions, Expr as SqlExpr, ObjectName, ObjectNamePart, SqlOption,
    Statement as SqlStatement, TableConstraint,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::sql::parse::ast::*;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
pub(crate) use crate::sql::parse::expr::translate_expr;
pub(crate) use crate::sql::parse::translate_type;

use crate::sql::parse::starrocks::StarRocksModel;
use crate::sql::storage::schema::{Engine, KeyModel, Value};

/// Parse one SQL string; exactly one statement expected (clients send one).
/// StarRocks table-model clauses lift out first (see `parse::starrocks`).
pub fn parse_statement(sql: &str) -> SqlResult<Statement> {
    let (text, model) = crate::sql::parse::starrocks::preparse(sql)?;
    // Legal MySQL `TRIM([BOTH|LEADING|TRAILING] FROM x)` gets its
    // implicit ' ' remstr made explicit: sqlparser's TRIM grammar
    // cannot take the keyword straight before FROM.
    let text = crate::sql::parse::trim_default::expand(&text);
    let stmts =
        Parser::parse_sql(&MySqlDialect {}, &text).map_err(|e| SqlError::parse(e.to_string()))?;
    match stmts.len() {
        0 => Err(SqlError::parse("empty statement")),
        1 => translate(stmts.into_iter().next().unwrap(), model.as_ref()),
        _ => Err(SqlError::unsupported("multi-statement queries")),
    }
}

/// Count `?` placeholders (prepared-statement parameter count).
pub fn placeholder_count(stmt: &Statement) -> usize {
    fn count_expr(x: &Expr) -> usize {
        match x {
            Expr::Placeholder => 1,
            Expr::Lit(_) | Expr::Col { .. } | Expr::InsertValues(_) => 0,
            Expr::Subquery(cq)
            | Expr::InSubquery { query: cq, .. }
            | Expr::Exists { query: cq, .. } => count_compound(cq),
            // Bind-time node: placeholders in keys/lhs bound above.
            Expr::Correlated { .. } => 0,
            Expr::BinaryOp { left, right, .. } => count_expr(left) + count_expr(right),
            Expr::Not(x) | Expr::Neg(x) => count_expr(x),
            Expr::IsNull { expr, .. } => count_expr(expr),
            Expr::InList { expr, list, .. } => {
                count_expr(expr) + list.iter().map(count_expr).sum::<usize>()
            }
            Expr::Between {
                expr, low, high, ..
            } => count_expr(expr) + count_expr(low) + count_expr(high),
            Expr::Like { expr, pattern, .. } => count_expr(expr) + count_expr(pattern),
            Expr::Agg { arg, .. } => arg.as_deref().map(count_expr).unwrap_or(0),
            Expr::Func { args, .. } => args.iter().map(count_expr).sum(),
            Expr::Case {
                operand,
                branches,
                else_expr,
            } => {
                operand.as_deref().map(count_expr).unwrap_or(0)
                    + branches
                        .iter()
                        .map(|(c, t)| count_expr(c) + count_expr(t))
                        .sum::<usize>()
                    + else_expr.as_deref().map(count_expr).unwrap_or(0)
            }
            Expr::Cast { expr, .. } => count_expr(expr),
            Expr::Regexp { expr, pattern, .. } => count_expr(expr) + count_expr(pattern),
        }
    }
    fn count_query(q: &Query) -> usize {
        q.items
            .iter()
            .filter_map(|i| match i {
                SelectItem::Expr { expr, .. } => Some(expr),
                SelectItem::Wildcard => None,
            })
            .map(count_expr)
            .sum::<usize>()
            + q.filter.as_ref().map(count_expr).unwrap_or(0)
            + q.group_by.iter().map(count_expr).sum::<usize>()
            + q.having.as_ref().map(count_expr).unwrap_or(0)
            + q.order_by
                .iter()
                .map(|k| count_expr(&k.expr))
                .sum::<usize>()
            // LIMIT / OFFSET parameters come last in the SQL text.
            + q.limit.as_ref().map(count_limit).unwrap_or(0)
            + count_limit(&q.offset)
    }
    fn count_limit(l: &LimitValue) -> usize {
        match l {
            LimitValue::Const(_) => 0,
            LimitValue::Param(e) => count_expr(e),
        }
    }
    fn count_compound(cq: &CompoundQuery) -> usize {
        cq.ctes
            .iter()
            .map(|c| count_compound(&c.query))
            .sum::<usize>()
            + count_body(&cq.body)
            + cq.order_by
                .iter()
                .map(|k| count_expr(&k.expr))
                .sum::<usize>()
            + cq.limit.as_ref().map(count_limit).unwrap_or(0)
            + count_limit(&cq.offset)
    }
    fn count_body(b: &QueryBody) -> usize {
        match b {
            QueryBody::Select(q) => count_query(q) + count_from(&q.from),
            QueryBody::Nested(inner) => count_compound(inner),
            QueryBody::SetOp { left, right, .. } => count_body(left) + count_body(right),
        }
    }
    fn count_from(t: &TableRef) -> usize {
        match t {
            TableRef::Table { .. } | TableRef::NoTable => 0,
            TableRef::Derived { query, .. } => count_compound(query),
            TableRef::Join { left, right, .. } => count_from(left) + count_from(right),
        }
    }
    match stmt {
        Statement::Select(q) => count_query(q) + count_from(&q.from),
        Statement::SelectCompound(cq) => count_compound(cq),
        Statement::Insert {
            source, conflict, ..
        } => {
            let rows = match source {
                InsertSource::Values(rows) => rows
                    .iter()
                    .flat_map(|r| r.iter().map(count_expr))
                    .sum::<usize>(),
                InsertSource::Select(cq) => count_compound(cq),
            };
            rows + conflict
                .assignments()
                .map(|a| a.iter().map(|(_, x)| count_expr(x)).sum::<usize>())
                .unwrap_or(0)
        }
        Statement::Update {
            table: _,
            assignments,
            filter,
            order_by,
            limit,
        } => {
            assignments
                .iter()
                .map(|(_, x)| count_expr(x))
                .sum::<usize>()
                + filter.as_ref().map(count_expr).unwrap_or(0)
                // ORDER BY / LIMIT trail the assignments and WHERE in
                // the statement text (positional bind order).
                + order_by
                    .iter()
                    .map(|k| count_expr(&k.expr))
                    .sum::<usize>()
                + limit.as_ref().map(count_limit).unwrap_or(0)
        }
        Statement::Delete {
            table: _,
            filter,
            order_by,
            limit,
        } => {
            filter.as_ref().map(count_expr).unwrap_or(0)
                + order_by.iter().map(|k| count_expr(&k.expr)).sum::<usize>()
                + limit.as_ref().map(count_limit).unwrap_or(0)
        }
        _ => 0,
    }
}

/// Substitute the i-th `?` with `v` (in-order across the whole statement).
pub fn bind_placeholders(stmt: &mut Statement, values: &[Value]) -> SqlResult<()> {
    let mut next = 0usize;
    let total = placeholder_count(stmt);
    if total != values.len() {
        return Err(SqlError::new(
            ErrorCode::WrongValueCount,
            format!("statement needs {total} parameters, got {}", values.len()),
        ));
    }
    fn bind(e: &mut Expr, next: &mut usize, values: &[Value]) {
        match e {
            Expr::Placeholder => {
                *e = Expr::Lit(values[*next].clone());
                *next += 1;
            }
            Expr::Lit(_) | Expr::Col { .. } | Expr::InsertValues(_) => {}
            Expr::Subquery(cq) => bind_compound(cq, next, values),
            Expr::InSubquery { expr, query, .. } => {
                bind(expr, next, values);
                bind_compound(query, next, values);
            }
            Expr::Exists { query, .. } => bind_compound(query, next, values),
            // Bind-time node; unreachable on parsed statements (kept
            // total for the exhaustive match).
            Expr::Correlated { kind, keys, .. } => {
                if let CorrelatedKind::In { lhs, .. } = kind {
                    bind(lhs, next, values);
                }
                for k in keys {
                    bind(k, next, values);
                }
            }
            Expr::BinaryOp { left, right, .. } => {
                bind(left, next, values);
                bind(right, next, values);
            }
            Expr::Not(x) | Expr::Neg(x) => bind(x, next, values),
            Expr::IsNull { expr, .. } => bind(expr, next, values),
            Expr::InList { expr, list, .. } => {
                bind(expr, next, values);
                for item in list {
                    bind(item, next, values);
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                bind(expr, next, values);
                bind(low, next, values);
                bind(high, next, values);
            }
            Expr::Like { expr, pattern, .. } => {
                bind(expr, next, values);
                bind(pattern, next, values);
            }
            Expr::Agg { arg, .. } => {
                if let Some(a) = arg {
                    bind(a, next, values);
                }
            }
            Expr::Func { args, .. } => {
                for a in args {
                    bind(a, next, values);
                }
            }
            Expr::Case {
                operand,
                branches,
                else_expr,
            } => {
                if let Some(o) = operand.as_mut() {
                    bind(o, next, values);
                }
                for (c, t) in branches.iter_mut() {
                    bind(c, next, values);
                    bind(t, next, values);
                }
                if let Some(e) = else_expr.as_mut() {
                    bind(e, next, values);
                }
            }
            Expr::Cast { expr, .. } => bind(expr, next, values),
            Expr::Regexp { expr, pattern, .. } => {
                bind(expr, next, values);
                bind(pattern, next, values);
            }
        }
    }
    fn bind_query(q: &mut Query, next: &mut usize, values: &[Value]) {
        for item in &mut q.items {
            if let SelectItem::Expr { expr, .. } = item {
                bind(expr, next, values);
            }
        }
        if let Some(f) = q.filter.as_mut() {
            bind(f, next, values);
        }
        for g in &mut q.group_by {
            bind(g, next, values);
        }
        if let Some(h) = q.having.as_mut() {
            bind(h, next, values);
        }
        for k in &mut q.order_by {
            bind(&mut k.expr, next, values);
        }
        // LIMIT / OFFSET placeholders bind last (they trail the rest
        // of the statement text), limit-then-offset -- the text order
        // of `LIMIT ? OFFSET ?`. The two-placeholder comma form
        // (`LIMIT ?, ?`, offset-first in text) is rejected at parse
        // time, so this order is never ambiguous.
        bind_limit(q.limit.as_mut(), next, values);
        bind_limit(Some(&mut q.offset), next, values);
    }
    fn bind_compound(cq: &mut CompoundQuery, next: &mut usize, values: &[Value]) {
        for cte in &mut cq.ctes {
            bind_compound(&mut cte.query, next, values);
        }
        bind_body(&mut cq.body, next, values);
        for k in &mut cq.order_by {
            bind(&mut k.expr, next, values);
        }
        bind_limit(cq.limit.as_mut(), next, values);
        bind_limit(Some(&mut cq.offset), next, values);
    }
    fn bind_limit(l: Option<&mut LimitValue>, next: &mut usize, values: &[Value]) {
        if let Some(LimitValue::Param(e)) = l {
            bind(e, next, values);
        }
    }
    fn bind_body(b: &mut QueryBody, next: &mut usize, values: &[Value]) {
        match b {
            QueryBody::Select(q) => {
                bind_query(q, next, values);
                bind_from(&mut q.from, next, values);
            }
            QueryBody::Nested(inner) => bind_compound(inner, next, values),
            QueryBody::SetOp { left, right, .. } => {
                bind_body(left, next, values);
                bind_body(right, next, values);
            }
        }
    }
    fn bind_from(t: &mut TableRef, next: &mut usize, values: &[Value]) {
        match t {
            TableRef::Table { .. } | TableRef::NoTable => {}
            TableRef::Derived { query, .. } => bind_compound(query, next, values),
            TableRef::Join { left, right, .. } => {
                bind_from(left, next, values);
                bind_from(right, next, values);
            }
        }
    }
    match stmt {
        Statement::Select(q) => {
            bind_query(q, &mut next, values);
            bind_from(&mut q.from, &mut next, values);
        }
        Statement::SelectCompound(cq) => bind_compound(cq, &mut next, values),
        Statement::Insert {
            source, conflict, ..
        } => {
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        for v in row {
                            bind(v, &mut next, values);
                        }
                    }
                }
                InsertSource::Select(cq) => bind_compound(cq, &mut next, values),
            }
            // ODKU assignment placeholders bind after the row sources
            // (they trail the VALUES/SELECT clause in the statement
            // text -- MySQL positional order).
            if let ConflictAction::OnDuplicate(assigns) = conflict {
                for (_, x) in assigns.iter_mut() {
                    bind(x, &mut next, values);
                }
            }
        }
        Statement::Update {
            table: _,
            assignments,
            filter,
            order_by,
            limit,
        } => {
            for (_, x) in assignments {
                bind(x, &mut next, values);
            }
            if let Some(f) = filter.as_mut() {
                bind(f, &mut next, values);
            }
            for k in order_by.iter_mut() {
                bind(&mut k.expr, &mut next, values);
            }
            bind_limit(limit.as_mut(), &mut next, values);
        }
        Statement::Delete {
            table: _,
            filter,
            order_by,
            limit,
        } => {
            if let Some(f) = filter.as_mut() {
                bind(f, &mut next, values);
            }
            for k in order_by.iter_mut() {
                bind(&mut k.expr, &mut next, values);
            }
            bind_limit(limit.as_mut(), &mut next, values);
        }
        _ => {}
    }
    debug_assert_eq!(next, values.len());
    Ok(())
}

pub(crate) fn object_name(name: &ObjectName) -> SqlResult<String> {
    let parts: Vec<String> = name
        .0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(id) => Ok(id.value.clone()),
            ObjectNamePart::Function(f) => Err(SqlError::unsupported(format!("name part {f}"))),
        })
        .collect::<SqlResult<Vec<_>>>()?;
    match parts.len() {
        1 => Ok(parts.into_iter().next().unwrap()),
        _ => Err(SqlError::unsupported(format!(
            "qualified name '{name}' (single-part names only)"
        ))),
    }
}

fn translate(stmt: SqlStatement, sr: Option<&StarRocksModel>) -> SqlResult<Statement> {
    match stmt {
        SqlStatement::Query(q) => {
            // Plain lock-bearing SELECT keeps the fast path; CTE /
            // UNION / parenthesized shapes go through the compound
            // query expression.
            if q.with.is_none() && matches!(q.body.as_ref(), sqlparser::ast::SetExpr::Select(_)) {
                Ok(Statement::Select(
                    crate::sql::parse::query::translate_query(&q)?,
                ))
            } else {
                Ok(Statement::SelectCompound(Box::new(
                    crate::sql::parse::query::translate_compound(&q)?,
                )))
            }
        }
        SqlStatement::Insert(i) => translate_insert(i),
        SqlStatement::Update(u) => super::translate_dml::translate_update(u),
        SqlStatement::Delete(d) => super::translate_dml::translate_delete(d),
        SqlStatement::CreateTable(c) => translate_create_table(c, sr),
        SqlStatement::Drop {
            object_type,
            if_exists,
            names,
            table,
            ..
        } => super::translate_ddl::translate_drop(object_type, if_exists, &names, table.as_ref()),
        SqlStatement::CreateIndex(c) => super::translate_ddl::translate_create_index(c),
        SqlStatement::Explain { statement, .. } => {
            Ok(Statement::Explain(Box::new(translate(*statement, sr)?)))
        }
        // DESCRIBE/DESC <table> is MySQL's SHOW COLUMNS spelling.
        SqlStatement::ExplainTable { table_name, .. } => {
            Ok(Statement::ShowColumns(object_name(&table_name)?))
        }
        SqlStatement::StartTransaction { .. } => Ok(Statement::Begin),
        SqlStatement::Commit { .. } => Ok(Statement::Commit),
        SqlStatement::Rollback { chain, savepoint } => {
            if chain {
                return Err(SqlError::unsupported("ROLLBACK AND CHAIN"));
            }
            match savepoint {
                None => Ok(Statement::Rollback),
                Some(name) => Ok(Statement::RollbackTo {
                    name: name.value.clone(),
                }),
            }
        }
        SqlStatement::Savepoint { name } => Ok(Statement::Savepoint(name.value.clone())),
        SqlStatement::ReleaseSavepoint { name } => Ok(Statement::ReleaseSavepoint {
            name: name.value.clone(),
        }),
        SqlStatement::Use(u) => match u {
            sqlparser::ast::Use::Database(db) | sqlparser::ast::Use::Object(db) => {
                Ok(Statement::Use(object_name(&db)?))
            }
            _ => Err(SqlError::unsupported("USE <object> other than database")),
        },
        SqlStatement::ShowTables { .. } => Ok(Statement::ShowTables),
        SqlStatement::ShowColumns { show_options, .. } => {
            let name = show_options
                .show_in
                .as_ref()
                .and_then(|in_| in_.parent_name.clone())
                .ok_or_else(|| SqlError::parse("SHOW COLUMNS FROM <table> required"))?;
            Ok(Statement::ShowColumns(object_name(&name)?))
        }
        // No dedicated SHOW INDEX parse: a ShowVariable [index, from, <table>].
        SqlStatement::ShowVariable { variable } if variable.len() == 3 => {
            let is_show_index = variable[0].value.eq_ignore_ascii_case("index")
                && variable[1].value.eq_ignore_ascii_case("from");
            if is_show_index {
                Ok(Statement::ShowIndexes(variable[2].value.clone()))
            } else {
                Err(SqlError::unsupported("SHOW variables"))
            }
        }
        SqlStatement::ShowDatabases { .. } => Ok(Statement::ShowDatabases),
        SqlStatement::ShowCreate { obj_type, obj_name } => {
            super::translate_ddl::translate_show_create(obj_type, &obj_name)
        }
        SqlStatement::ShowVariables { filter, .. } => {
            super::translate_ddl::translate_show_filter(filter, true)
        }
        SqlStatement::ShowStatus { filter, .. } => {
            super::translate_ddl::translate_show_filter(filter, false)
        }
        SqlStatement::Truncate(t) => super::translate_ddl::translate_truncate(t),
        SqlStatement::RenameTable(r) => super::translate_ddl::translate_rename(&r),
        SqlStatement::AlterTable(a) => super::translate_ddl::translate_alter_table(a),
        SqlStatement::Set(set) => translate_set(set),
        other => Err(SqlError::unsupported(format!("{other}"))),
    }
}

/// Session-state SET statements are rejected loudly (silently
/// ignoring them would mislead clients about autocommit/isolation
/// semantics); cosmetic assignments (`sql_mode`, `wait_timeout`, ...)
/// are accepted and ignored.
fn translate_set(set: sqlparser::ast::Set) -> SqlResult<Statement> {
    use sqlparser::ast::Set as SqlSet;
    match set {
        SqlSet::SetTransaction { modes, .. } => translate_set_transaction(&modes),
        // `SET NAMES x [COLLATE y]` / `SET NAMES DEFAULT`: pure charset
        // declaration. Real clients (mycli/pymysql/JDBC) send it on every
        // connect; the engine is utf-8 end-to-end, so honor it as a no-op
        // instead of breaking the handshake.
        SqlSet::SetNames { .. } | SqlSet::SetNamesDefault {} => Ok(Statement::SetIgnored),
        SqlSet::SetTimeZone { .. }
        | SqlSet::SetRole { .. }
        | SqlSet::SetSessionAuthorization(_)
        | SqlSet::SetSessionParam(_)
        | SqlSet::ParenthesizedAssignments { .. } => Err(SqlError::unsupported(format!(
            "{set} (session/transaction settings)"
        ))),
        SqlSet::SingleAssignment {
            ref variable,
            ref values,
            ..
        } => super::session::reject_session_var(variable, values, &set),
        SqlSet::MultipleAssignments {
            ref assignments, ..
        } => {
            for a in assignments {
                super::session::reject_session_var(&a.name, std::slice::from_ref(&a.value), &set)?;
            }
            Ok(Statement::SetIgnored)
        }
    }
}

fn translate_set_transaction(modes: &[sqlparser::ast::TransactionMode]) -> SqlResult<Statement> {
    use sqlparser::ast::{TransactionAccessMode, TransactionIsolationLevel, TransactionMode};
    let mut level: Option<String> = None;
    for m in modes {
        match m {
            TransactionMode::IsolationLevel(l) => {
                let name = match l {
                    TransactionIsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
                    TransactionIsolationLevel::ReadCommitted => "READ COMMITTED",
                    TransactionIsolationLevel::RepeatableRead => "REPEATABLE READ",
                    TransactionIsolationLevel::Serializable => "SERIALIZABLE",
                    TransactionIsolationLevel::Snapshot => "SNAPSHOT",
                };
                level = Some(name.to_string());
            }
            TransactionMode::AccessMode(TransactionAccessMode::ReadOnly) => {
                return Err(SqlError::unsupported("READ ONLY transactions"))
            }
            // READ WRITE is the engine's only mode; accepting the
            // explicit keyword is a no-op.
            TransactionMode::AccessMode(TransactionAccessMode::ReadWrite) => {}
        }
    }
    match level {
        // A no-isolation `SET TRANSACTION READ WRITE` is harmless.
        None => Ok(Statement::SetIgnored),
        Some(level) => Ok(Statement::SetIsolation { level }),
    }
}

fn translate_create_table(
    c: sqlparser::ast::CreateTable,
    sr: Option<&StarRocksModel>,
) -> SqlResult<Statement> {
    if c.query.is_some() {
        return Err(SqlError::unsupported("CREATE TABLE ... AS SELECT"));
    }
    let name = object_name(&c.name)?;
    let mut columns = Vec::new();
    let mut inline_pk: Option<String> = None;
    for col in &c.columns {
        let (spec, pk) = translate_type::translate_column(col)?;
        if pk {
            if inline_pk.is_some() {
                return Err(SqlError::unsupported("multiple inline PRIMARY KEY columns"));
            }
            inline_pk = Some(spec.name.clone());
        }
        columns.push(spec);
    }
    // Pk columns in declaration order: inline `col ... PRIMARY KEY`
    // contributes one; a `PRIMARY KEY (a, b, ...)` table constraint
    // contributes its whole list (multi-column pks, composite support).
    // Mixing the two forms, or two constraints, still rejects. `KEY` /
    // `UNIQUE KEY` constraints are inline secondary indexes (M4).
    let mut pk: Vec<String> = inline_pk.into_iter().collect();
    let mut indexes = Vec::new();
    for constraint in &c.constraints {
        match constraint {
            TableConstraint::PrimaryKey(cons) => {
                if !pk.is_empty() {
                    return Err(SqlError::unsupported("multiple PRIMARY KEY definitions"));
                }
                for col in &cons.columns {
                    pk.push(match &col.column.expr {
                        SqlExpr::Identifier(id) => id.value.clone(),
                        other => return Err(SqlError::unsupported(format!("PRIMARY KEY {other}"))),
                    });
                }
            }
            TableConstraint::Index(cons) => {
                let column = super::translate_ddl::index_column_of(&cons.columns)?;
                indexes.push(InlineIndex {
                    name: cons
                        .name
                        .as_ref()
                        .map(|n| n.value.clone())
                        .unwrap_or_else(|| format!("idx_{column}")),
                    column,
                    unique: false,
                });
            }
            TableConstraint::Unique(cons) => {
                let column = super::translate_ddl::index_column_of(&cons.columns)?;
                let name = cons
                    .index_name
                    .as_ref()
                    .map(|n| n.value.clone())
                    .or_else(|| cons.name.as_ref().map(|n| n.value.clone()))
                    .unwrap_or_else(|| format!("idx_{column}"));
                indexes.push(InlineIndex {
                    name,
                    column,
                    unique: true,
                });
            }
            _ => return Err(SqlError::unsupported("other table constraints")),
        }
    }
    if pk.is_empty() {
        // DUPLICATE model: StarRocks has no pk; the first dup-key column
        // becomes the schema pk (metadata only -- the columnar engine
        // never dedups on it, see ddl::build_schema).
        if sr.is_some_and(|m| m.kind == KeyModel::Duplicate) {
            pk.push(sr.unwrap().keys[0].clone());
        } else {
            return Err(SqlError::unsupported("need a PRIMARY KEY"));
        }
    }
    // Every pk column must exist exactly once, and a composite pk may
    // not name the same column twice.
    let mut seen: Vec<&str> = Vec::new();
    for p in &pk {
        let hits = columns
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case(p))
            .count();
        if hits != 1 {
            return Err(SqlError::parse(format!(
                "PRIMARY KEY column '{p}' not defined"
            )));
        }
        if seen.iter().any(|s| s.eq_ignore_ascii_case(p)) {
            return Err(SqlError::parse(format!(
                "duplicate column '{p}' in primary key"
            )));
        }
        seen.push(p);
    }
    Ok(Statement::CreateTable {
        name,
        if_not_exists: c.if_not_exists,
        columns,
        pk,
        engine: table_engine(&c.table_options),
        starrocks: sr.cloned(),
        indexes,
    })
}

/// MySQL `ENGINE=...` table option -> storage engine. sqlparser hands
/// the plain option list over as [`SqlOption::NamedParenthesizedList`]
/// entries; anything but `ENGINE=columnar` (absent, other keys, other
/// values) falls back to [`Engine::Row`] -- accepted and ignored,
/// MySQL-style.
fn table_engine(opts: &CreateTableOptions) -> Engine {
    let options = match opts {
        CreateTableOptions::Plain(o)
        | CreateTableOptions::With(o)
        | CreateTableOptions::Options(o) => o,
        _ => return Engine::Row,
    };
    for opt in options {
        let SqlOption::NamedParenthesizedList(npl) = opt else {
            continue;
        };
        if npl.key.value.eq_ignore_ascii_case("engine") {
            return match &npl.name {
                Some(id) if id.value.eq_ignore_ascii_case("columnar") => Engine::Columnar,
                _ => Engine::Row,
            };
        }
    }
    Engine::Row
}

fn translate_insert(i: sqlparser::ast::Insert) -> SqlResult<Statement> {
    super::translate_dml::translate_insert(i)
}

/// ORDER BY keys shared by UPDATE/DELETE.
pub(crate) fn translate_order(keys: &[sqlparser::ast::OrderByExpr]) -> SqlResult<Vec<OrderKey>> {
    keys.iter()
        .map(|k| {
            Ok(OrderKey {
                expr: translate_expr(&k.expr)?,
                asc: k.options.asc.unwrap_or(true),
            })
        })
        .collect()
}
