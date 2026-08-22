//! sqlparser (MySQL dialect) AST -> internal IR.
//!
//! Parse with [`parse_statement`]; anything the v1 engine cannot run is
//! rejected here with an explicit `not supported` / parse error so the
//! executor only ever sees runnable shapes.

use sqlparser::ast::{
    ColumnDef as SqlColumnDef, ColumnOption, DataType, Expr as SqlExpr, FromTable, ObjectName,
    ObjectNamePart, SetExpr, Statement as SqlStatement, TableConstraint, TableFactor, TableObject,
    TableWithJoins,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

use crate::sql::parse::ast::*;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
pub(crate) use crate::sql::parse::expr::translate_expr;

use crate::sql::storage::schema::{SqlType, Value};

/// Parse one SQL string; exactly one statement expected (clients send one).
pub fn parse_statement(sql: &str) -> SqlResult<Statement> {
    let stmts =
        Parser::parse_sql(&MySqlDialect {}, sql).map_err(|e| SqlError::parse(e.to_string()))?;
    match stmts.len() {
        0 => Err(SqlError::parse("empty statement")),
        1 => translate(stmts.into_iter().next().unwrap()),
        _ => Err(SqlError::unsupported("multi-statement queries")),
    }
}

/// Count `?` placeholders (prepared-statement parameter count).
pub fn placeholder_count(stmt: &Statement) -> usize {
    fn count_expr(x: &Expr) -> usize {
        match x {
            Expr::Placeholder => 1,
            Expr::Lit(_) | Expr::Col { .. } => 0,
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
    }
    match stmt {
        Statement::Select(q) => count_query(q),
        Statement::Insert { rows, .. } => rows.iter().flat_map(|r| r.iter().map(count_expr)).sum(),
        Statement::Update {
            assignments,
            filter,
            ..
        } => {
            assignments
                .iter()
                .map(|(_, x)| count_expr(x))
                .sum::<usize>()
                + filter.as_ref().map(count_expr).unwrap_or(0)
        }
        Statement::Delete { filter, .. } => filter.as_ref().map(count_expr).unwrap_or(0),
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
            Expr::Lit(_) | Expr::Col { .. } => {}
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
    }
    match stmt {
        Statement::Select(q) => bind_query(q, &mut next, values),
        Statement::Insert { rows, .. } => {
            for row in rows {
                for v in row {
                    bind(v, &mut next, values);
                }
            }
        }
        Statement::Update {
            assignments,
            filter,
            ..
        } => {
            for (_, x) in assignments {
                bind(x, &mut next, values);
            }
            if let Some(f) = filter.as_mut() {
                bind(f, &mut next, values);
            }
        }
        Statement::Delete { filter, .. } => {
            if let Some(f) = filter.as_mut() {
                bind(f, &mut next, values);
            }
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

fn translate(stmt: SqlStatement) -> SqlResult<Statement> {
    match stmt {
        SqlStatement::Query(q) => Ok(Statement::Select(
            crate::sql::parse::query::translate_query(&q)?,
        )),
        SqlStatement::Insert(i) => translate_insert(i),
        SqlStatement::Update(u) => translate_update(u),
        SqlStatement::Delete(d) => translate_delete(d),
        SqlStatement::CreateTable(c) => translate_create_table(c),
        SqlStatement::Drop {
            object_type,
            if_exists,
            names,
            table,
            ..
        } => translate_drop(object_type, if_exists, &names, table.as_ref()),
        SqlStatement::CreateIndex(c) => {
            if c.columns.len() != 1 {
                return Err(SqlError::unsupported("multi-column indexes"));
            }
            let col = match &c.columns[0].column.expr {
                SqlExpr::Identifier(id) => id.value.clone(),
                other => return Err(SqlError::unsupported(format!("index key {other}"))),
            };
            Ok(Statement::CreateIndex {
                table: object_name(&c.table_name)?,
                name: c
                    .name
                    .as_ref()
                    .map(object_name)
                    .transpose()?
                    .unwrap_or_else(|| format!("idx_{col}")),
                column: col,
                unique: c.unique,
                if_not_exists: c.if_not_exists,
            })
        }
        SqlStatement::Explain { statement, .. } => {
            Ok(Statement::Explain(Box::new(translate(*statement)?)))
        }
        SqlStatement::StartTransaction { .. } => Ok(Statement::Begin),
        SqlStatement::Commit { .. } => Ok(Statement::Commit),
        SqlStatement::Rollback { .. } => Ok(Statement::Rollback),
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
        // sqlparser has no dedicated SHOW INDEX parse: it surfaces as a
        // ShowVariable whose identifiers are [index, from, <table>].
        SqlStatement::ShowVariable { variable } if variable.len() == 3 => {
            let is_show_index = variable[0].value.eq_ignore_ascii_case("index")
                && variable[1].value.eq_ignore_ascii_case("from");
            if is_show_index {
                Ok(Statement::ShowIndexes(variable[2].value.clone()))
            } else {
                Err(SqlError::unsupported("SHOW variables"))
            }
        }
        SqlStatement::Set(_) => Ok(Statement::SetIgnored),
        other => Err(SqlError::unsupported(format!("{other}"))),
    }
}

fn translate_drop(
    object_type: sqlparser::ast::ObjectType,
    if_exists: bool,
    names: &[ObjectName],
    table: Option<&ObjectName>,
) -> SqlResult<Statement> {
    if names.len() != 1 {
        return Err(SqlError::unsupported("dropping multiple objects"));
    }
    let name = object_name(&names[0])?;
    match object_type {
        sqlparser::ast::ObjectType::Table if table.is_none() => {
            Ok(Statement::DropTable { name, if_exists })
        }
        sqlparser::ast::ObjectType::Index => {
            // MySQL: DROP INDEX idx ON tbl
            let table = table
                .map(object_name)
                .transpose()?
                .ok_or_else(|| SqlError::parse("DROP INDEX needs ON <table>"))?;
            Ok(Statement::DropIndex {
                table,
                name,
                if_exists,
            })
        }
        other => Err(SqlError::unsupported(format!("DROP {other}"))),
    }
}

fn translate_create_table(c: sqlparser::ast::CreateTable) -> SqlResult<Statement> {
    if c.query.is_some() {
        return Err(SqlError::unsupported("CREATE TABLE ... AS SELECT"));
    }
    let name = object_name(&c.name)?;
    let mut columns = Vec::new();
    let mut inline_pk: Option<String> = None;
    for col in &c.columns {
        let (spec, pk) = translate_column(col)?;
        if pk {
            inline_pk = Some(spec.name.clone());
        }
        columns.push(spec);
    }
    let mut pk = inline_pk;
    for constraint in &c.constraints {
        match constraint {
            TableConstraint::PrimaryKey(cons) => {
                if pk.is_some() || cons.columns.len() != 1 {
                    return Err(SqlError::unsupported(
                        "exactly one primary-key column is required",
                    ));
                }
                pk = Some(match &cons.columns[0].column.expr {
                    SqlExpr::Identifier(id) => id.value.clone(),
                    other => return Err(SqlError::unsupported(format!("PRIMARY KEY {other}"))),
                });
            }
            _ => {
                return Err(SqlError::unsupported(
                    "table constraints other than PRIMARY KEY",
                ))
            }
        }
    }
    let Some(pk) = pk else {
        return Err(SqlError::unsupported(
            "tables need exactly one PRIMARY KEY column",
        ));
    };
    if columns
        .iter()
        .filter(|c| c.name.eq_ignore_ascii_case(&pk))
        .count()
        != 1
    {
        return Err(SqlError::parse(format!(
            "PRIMARY KEY column '{pk}' not defined"
        )));
    }
    Ok(Statement::CreateTable {
        name,
        if_not_exists: c.if_not_exists,
        columns,
        pk,
    })
}

fn translate_column(col: &SqlColumnDef) -> SqlResult<(ColumnSpec, bool)> {
    let sql_type = translate_type(&col.data_type)?;
    let mut nullable = true;
    let mut pk = false;
    for opt in &col.options {
        match &opt.option {
            ColumnOption::Null => nullable = true,
            ColumnOption::NotNull => nullable = false,
            ColumnOption::PrimaryKey(_) => {
                pk = true;
                nullable = false;
            }
            ColumnOption::Unique(_) => {} // use CREATE UNIQUE INDEX instead
            ColumnOption::Default(_) => {} // defaults are client-evaluated in v1
            ColumnOption::Comment(_) => {}
            ColumnOption::DialectSpecific(_)
                if opt.to_string().eq_ignore_ascii_case("AUTO_INCREMENT") => {}
            _ => {
                return Err(SqlError::unsupported(format!(
                    "column option {}",
                    opt.option
                )))
            }
        }
    }
    Ok((
        ColumnSpec {
            name: col.name.value.clone(),
            sql_type,
            nullable,
        },
        pk,
    ))
}

fn translate_type(t: &DataType) -> SqlResult<SqlType> {
    use DataType::*;
    Ok(match t {
        Bool | Boolean => SqlType::Bool,
        TinyInt(_) | Int2(_) | SmallInt(_) | MediumInt(_) | Int(_) | Int4(_) | Integer(_)
        | Int8(_) | BigInt(_) => SqlType::Int,
        Float(_) | Float4 | Real | Double(_) | Float8 | DoublePrecision => SqlType::Double,
        Varchar(_) | CharVarying(_) | Char(_) | Character(_) | CharacterVarying(_) | Text
        | TinyText | MediumText | LongText | String(_) => SqlType::VarChar,
        Varbinary(_) | Binary(_) | Blob(_) | TinyBlob | MediumBlob | LongBlob | Bytea => {
            SqlType::Blob
        }
        other => {
            return Err(SqlError::unsupported(format!(
                "column type {other} (v1: BOOL/INT/DOUBLE/VARCHAR/BLOB)"
            )))
        }
    })
}

fn translate_insert(i: sqlparser::ast::Insert) -> SqlResult<Statement> {
    if i.on.is_some() {
        return Err(SqlError::unsupported(
            "ON DUPLICATE KEY UPDATE / ON CONFLICT",
        ));
    }
    if !i.assignments.is_empty() {
        return Err(SqlError::unsupported("INSERT ... SET"));
    }
    if i.replace_into {
        return Err(SqlError::unsupported("REPLACE INTO"));
    }
    let table = match &i.table {
        TableObject::TableName(n) => object_name(n)?,
        other => return Err(SqlError::unsupported(format!("INSERT target {other}"))),
    };
    let columns = i
        .columns
        .iter()
        .map(object_name)
        .collect::<SqlResult<Vec<String>>>()?;
    let source = i
        .source
        .ok_or_else(|| SqlError::parse("INSERT needs VALUES"))?;
    let SetExpr::Values(values) = source.body.as_ref() else {
        return Err(SqlError::unsupported("INSERT source other than VALUES"));
    };
    if source.with.is_some() {
        return Err(SqlError::unsupported("INSERT with CTE"));
    }
    let rows = values
        .rows
        .iter()
        .map(|row| row.iter().map(translate_expr).collect())
        .collect::<SqlResult<Vec<Vec<Expr>>>>()?;
    for row in &rows {
        if row.is_empty() || (!columns.is_empty() && row.len() != columns.len()) {
            return Err(SqlError::new(
                ErrorCode::WrongValueCount,
                "INSERT row arity does not match column list",
            ));
        }
    }
    Ok(Statement::Insert {
        table,
        columns,
        rows,
    })
}

fn translate_update(u: sqlparser::ast::Update) -> SqlResult<Statement> {
    if u.from.is_some() || u.returning.is_some() || u.or.is_some() {
        return Err(SqlError::unsupported("UPDATE with FROM/RETURNING/OR"));
    }
    let name = single_table(&u.table)?;
    let assignments = u
        .assignments
        .iter()
        .map(|a| {
            let sqlparser::ast::AssignmentTarget::ColumnName(n) = &a.target else {
                return Err(SqlError::unsupported("tuple assignment targets"));
            };
            let name = object_name(n)?;
            Ok((name, translate_expr(&a.value)?))
        })
        .collect::<SqlResult<Vec<(String, Expr)>>>()?;
    Ok(Statement::Update {
        table: name,
        assignments,
        filter: u.selection.as_ref().map(translate_expr).transpose()?,
        order_by: translate_order(&u.order_by)?,
        limit: translate_limit(&u.limit)?,
    })
}

fn translate_delete(d: sqlparser::ast::Delete) -> SqlResult<Statement> {
    if d.using.as_ref().is_some_and(|u| !u.is_empty()) {
        return Err(SqlError::unsupported("DELETE ... USING"));
    }
    if !d.tables.is_empty() || d.returning.is_some() {
        return Err(SqlError::unsupported("multi-table DELETE / RETURNING"));
    }
    let name = match &d.from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => {
            if tables.len() != 1 {
                return Err(SqlError::unsupported("multi-table DELETE"));
            }
            single_table(&tables[0])?
        }
    };
    Ok(Statement::Delete {
        table: name,
        filter: d.selection.as_ref().map(translate_expr).transpose()?,
        order_by: translate_order(&d.order_by)?,
        limit: translate_limit(&d.limit)?,
    })
}

/// UPDATE/DELETE target: exactly one plain table, no alias.
fn single_table(t: &TableWithJoins) -> SqlResult<String> {
    if !t.joins.is_empty() {
        return Err(SqlError::unsupported("DML across a JOIN"));
    }
    single_factor(&t.relation)
}

fn single_factor(f: &TableFactor) -> SqlResult<String> {
    match f {
        TableFactor::Table { name, alias, .. } => {
            if alias.is_some() {
                return Err(SqlError::unsupported("table aliases in DML"));
            }
            object_name(name)
        }
        other => Err(SqlError::unsupported(format!("table reference {other}"))),
    }
}

/// `LIMIT <n>` on UPDATE/DELETE (plain Expr position).
pub(crate) fn translate_limit(e: &Option<SqlExpr>) -> SqlResult<Option<u64>> {
    match e {
        None => Ok(None),
        Some(SqlExpr::Value(v)) => match &v.value {
            sqlparser::ast::Value::Number(n, _) => n
                .parse::<u64>()
                .map(Some)
                .map_err(|_| SqlError::parse("LIMIT must be a non-negative integer")),
            _ => Err(SqlError::parse("LIMIT must be a non-negative integer")),
        },
        Some(_) => Err(SqlError::parse("LIMIT must be a non-negative integer")),
    }
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
