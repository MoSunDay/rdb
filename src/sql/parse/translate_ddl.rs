//! DDL-surface translation (M4): TRUNCATE / RENAME / ALTER TABLE
//! index+rename forms and the SHOW metadata statements sqlparser hands
//! over as dedicated variants. `translate.rs` dispatches here so the
//! shared walker surface stays narrow.
//!
//! Semantics fixed here:
//! - index keys are SINGLE-COLUMN only: composite keys and prefix keys
//!   (`col(10)`) reject loudly (P2 deferral, gap-matrix D group);
//! - `ALTER TABLE` supports ADD [UNIQUE] INDEX/KEY and DROP INDEX and
//!   RENAME [TO|AS]; every other operation (ADD/MODIFY/DROP COLUMN, ...)
//!   stays the loud P2 deferred rejection;
//! - SHOW ... [LIKE 'pat'] accepts the LIKE filter; the WHERE form
//!   rejects (no expression engine over metadata rowsets).

use sqlparser::ast::{Expr as SqlExpr, ObjectName, ShowStatementFilter, Value as SqlValue};

use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::parse::translate::object_name;

/// Shared single-column extraction over sqlparser `IndexColumn`s (the
/// CREATE TABLE inline constraints use the same shape).
pub(crate) fn index_column_of(cols: &[sqlparser::ast::IndexColumn]) -> SqlResult<String> {
    single_index_column(cols)
}

pub(crate) fn translate_drop(
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

/// `CREATE [UNIQUE] INDEX [name] ON t (col)`: one plain column key.
pub(crate) fn translate_create_index(c: sqlparser::ast::CreateIndex) -> SqlResult<Statement> {
    // The vector-DDL rewrite injected `vec_ann(col, 'metric', nprobe)`
    // as the sole "column"; lift it back into CreateVectorIndex.
    if let Some(v) = marker_vector_index(&c) {
        return Ok(v);
    }
    let column = single_index_column(&c.columns)?;
    Ok(Statement::CreateIndex {
        table: object_name(&c.table_name)?,
        name: c
            .name
            .as_ref()
            .map(object_name)
            .transpose()?
            .unwrap_or_else(|| format!("idx_{column}")),
        column,
        unique: c.unique,
        if_not_exists: c.if_not_exists,
    })
}

/// The one column an index key covers: a bare identifier. Composite
/// lists and prefix/expression keys (`col(10)`, `LOWER(x)`) reject --
/// the M2 index encodes exactly one column value.
fn single_index_column(cols: &[sqlparser::ast::IndexColumn]) -> SqlResult<String> {
    if cols.len() != 1 {
        return Err(SqlError::unsupported(
            "multi-column or prefix indexes (single-column indexes only)",
        ));
    }
    match &cols[0].column.expr {
        SqlExpr::Identifier(id) => Ok(id.value.clone()),
        other => Err(SqlError::unsupported(format!(
            "index key {other} (single plain columns only)"
        ))),
    }
}

/// `TRUNCATE [TABLE] t` for exactly one table, plain MySQL form (no
/// partitions / ON CLUSTER / Postgres identity options).
pub(crate) fn translate_truncate(t: sqlparser::ast::Truncate) -> SqlResult<Statement> {
    if t.table_names.len() != 1 {
        return Err(SqlError::unsupported("multi-table TRUNCATE"));
    }
    if t.partitions.is_some() || t.on_cluster.is_some() || t.identity.is_some() {
        return Err(SqlError::unsupported(
            "TRUNCATE with PARTITION/ON CLUSTER/IDENTITY options",
        ));
    }
    if t.if_exists {
        // MySQL has no TRUNCATE IF EXISTS; keep the same loud answer.
        return Err(SqlError::parse("TRUNCATE IF EXISTS is not valid MySQL"));
    }
    Ok(Statement::TruncateTable {
        name: object_name(&t.table_names[0].name)?,
    })
}

/// `RENAME TABLE a TO b` (exactly one pair; MySQL allows a list, the
/// multi-rename form is deferred with the other compound DDL).
pub(crate) fn translate_rename(rs: &[sqlparser::ast::RenameTable]) -> SqlResult<Statement> {
    if rs.len() != 1 {
        return Err(SqlError::unsupported("multi-pair RENAME TABLE"));
    }
    rename_of(&rs[0].old_name, &rs[0].new_name)
}

/// Reject anything but single-part names here (the engine is a
/// single-database model; cross-db renames reject loudly).
fn rename_of(from: &ObjectName, to: &ObjectName) -> SqlResult<Statement> {
    Ok(Statement::RenameTable {
        from: object_name(from)?,
        to: object_name(to)?,
    })
}

/// `ALTER TABLE t <op>`: the supported operations map onto the plain
/// CREATE INDEX / DROP INDEX / RENAME statements (same executors);
/// everything else names itself in the rejection.
pub(crate) fn translate_alter_table(a: sqlparser::ast::AlterTable) -> SqlResult<Statement> {
    if a.if_exists || a.only || a.on_cluster.is_some() || a.table_type.is_some() {
        return Err(SqlError::unsupported("ALTER TABLE IF EXISTS/ONLY variants"));
    }
    if a.operations.len() != 1 {
        return Err(SqlError::unsupported(
            "ALTER TABLE with multiple operations",
        ));
    }
    use sqlparser::ast::AlterTableOperation as Op;
    use sqlparser::ast::TableConstraint as Cons;
    let table = object_name(&a.name)?;
    match a.operations.into_iter().next().expect("len checked") {
        Op::AddConstraint { constraint, .. } => match constraint {
            Cons::Index(idx) => {
                // `ADD INDEX [name] (col)`: an IndexConstraint with the
                // name slot spelled `idx.name`.
                let column = single_index_column(&idx.columns)?;
                Ok(Statement::CreateIndex {
                    name: idx
                        .name
                        .map(|n| n.value)
                        .unwrap_or_else(|| format!("idx_{column}")),
                    table,
                    column,
                    unique: false,
                    if_not_exists: false,
                })
            }
            Cons::Unique(cons) => {
                // `ADD UNIQUE [INDEX|KEY] [name] (col)`.
                let column = single_index_column(&cons.columns)?;
                let name = cons
                    .index_name
                    .map(|n| n.value)
                    .or_else(|| cons.name.map(|n| n.value))
                    .unwrap_or_else(|| format!("idx_{column}"));
                Ok(Statement::CreateIndex {
                    table,
                    name,
                    column,
                    unique: true,
                    if_not_exists: false,
                })
            }
            other => Err(SqlError::unsupported(format!(
                "ALTER TABLE ADD {other} (P2 deferred)"
            ))),
        },
        Op::DropIndex { name } => Ok(Statement::DropIndex {
            table,
            name: name.value,
            if_exists: false,
        }),
        Op::RenameTable { table_name } => {
            use sqlparser::ast::RenameTableNameKind as Kind;
            let to = match table_name {
                Kind::To(n) | Kind::As(n) => n,
            };
            rename_of(&a.name, &to)
        }
        other => Err(SqlError::unsupported(format!(
            "ALTER TABLE {other} (P2 deferred: schema migration is its own plan)"
        ))),
    }
}

/// `SHOW CREATE TABLE t` (other object kinds reject loudly).
pub(crate) fn translate_show_create(
    obj_type: sqlparser::ast::ShowCreateObject,
    obj_name: &ObjectName,
) -> SqlResult<Statement> {
    match obj_type {
        sqlparser::ast::ShowCreateObject::Table => {
            Ok(Statement::ShowCreateTable(object_name(obj_name)?))
        }
        other => Err(SqlError::unsupported(format!("SHOW CREATE {other}"))),
    }
}

/// Shared `[LIKE 'pat' | WHERE expr]` filter of SHOW VARIABLES/STATUS:
/// LIKE keeps its pattern (matched at exec, MySQL SHOW convention is
/// case-insensitive), WHERE rejects.
pub(crate) fn translate_show_filter(
    filter: Option<ShowStatementFilter>,
    variables: bool,
) -> SqlResult<Statement> {
    let like = match filter {
        None => None,
        Some(ShowStatementFilter::Like(pattern)) => Some(pattern),
        Some(ShowStatementFilter::Where(_)) => {
            return Err(SqlError::unsupported("SHOW ... WHERE (LIKE only)"))
        }
        Some(other) => return Err(SqlError::unsupported(format!("SHOW filter {other:?}"))),
    };
    Ok(if variables {
        Statement::ShowVariables { like }
    } else {
        Statement::ShowStatus { like }
    })
}

#[cfg(test)]
#[path = "translate_ddl_tests.rs"]
mod tests;

/// Recognize the `vec_ann(col, 'metric', nprobe)` marker the
/// vector-DDL pre-parser injects (see `parse::vector_ddl`). Returns
/// None for every plain index -- the marker name is never a callable
/// function, so a user cannot reach this arm by hand.
fn marker_vector_index(c: &sqlparser::ast::CreateIndex) -> Option<Statement> {
    if c.columns.len() != 1 {
        return None;
    }
    let SqlExpr::Function(f) = &c.columns[0].column.expr else {
        return None;
    };
    let name = f.name.to_string().to_ascii_lowercase();
    if name != crate::sql::parse::vector_ddl::MARKER {
        return None;
    }
    let list = match &f.args {
        sqlparser::ast::FunctionArguments::List(l) => l,
        _ => return None,
    };
    let plain: Vec<&SqlExpr> = list
        .args
        .iter()
        .filter_map(|a| match a {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => {
                Some(e)
            }
            _ => None,
        })
        .collect();
    if plain.len() != 3 {
        return None;
    }
    let args = &plain;
    let column = match args[0] {
        SqlExpr::Identifier(id) => id.value.clone(),
        _ => return None,
    };
    let metric = match args[1] {
        SqlExpr::Value(v) if matches!(&v.value, SqlValue::SingleQuotedString(_)) => {
            let SqlValue::SingleQuotedString(m) = &v.value else {
                return None;
            };
            crate::sql::index::vector::Metric::parse(m)?
        }
        _ => return None,
    };
    let nprobe = match args[2] {
        SqlExpr::Value(v) if matches!(&v.value, SqlValue::Number(_, _)) => {
            let SqlValue::Number(n, _) = &v.value else {
                return None;
            };
            n.parse().ok()?
        }
        _ => return None,
    };
    Some(Statement::CreateVectorIndex {
        table: object_name(&c.table_name).ok()?,
        name: c
            .name
            .as_ref()
            .map(object_name)
            .transpose()
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("vec_{column}")),
        column,
        metric,
        nprobe,
        if_not_exists: c.if_not_exists,
    })
}
