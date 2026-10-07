//! DML translation: the INSERT family (M2). Owns plain INSERT,
//! `REPLACE INTO`, `ON DUPLICATE KEY UPDATE`, `INSERT ... SET` and
//! `INSERT ... SELECT` source shapes; `translate.rs` dispatches here so
//! the shared walker/translate surface stays narrow.
//!
//! MySQL semantics kept at parse time:
//! - `VALUES(col)` is legal ONLY inside ODKU assignments (it refers to
//!   the incoming row); anywhere else it is a parse error. Inside an
//!   assignment it becomes the IR marker [`Expr::InsertValues`].
//! - `INSERT ... SET a = e, b = e` is exactly a named single-row
//!   VALUES list, normalized here.
//! - `REPLACE INTO` + `ON DUPLICATE KEY UPDATE` together is a parse
//!   error (MySQL rejects the combination too).

use sqlparser::ast::SetExpr;

use crate::sql::parse::ast::{ConflictAction, Expr, InsertSource, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::parse::expr::translate_expr;
use crate::sql::parse::query::translate_compound;
use crate::sql::parse::translate::object_name;

/// Translate one sqlparser `Insert` into the IR `Statement::Insert`.
pub(crate) fn translate_insert(i: sqlparser::ast::Insert) -> SqlResult<Statement> {
    if let Some(or) = &i.or {
        return Err(SqlError::unsupported(format!("INSERT OR {or}")));
    }
    if i.insert_alias.is_some() {
        return Err(SqlError::unsupported("INSERT row aliases"));
    }
    let table = match &i.table {
        sqlparser::ast::TableObject::TableName(n) => object_name(n)?,
        other => return Err(SqlError::unsupported(format!("INSERT target {other}"))),
    };
    let conflict = conflict_action(&i)?;
    let (columns, source) = source_of(&i)?;
    Ok(Statement::Insert {
        table,
        columns,
        source,
        conflict,
    })
}

/// Conflict clause: `ON DUPLICATE KEY UPDATE ...` (assignments with the
/// `VALUES(col)` marker), `REPLACE INTO`, or the plain default.
fn conflict_action(i: &sqlparser::ast::Insert) -> SqlResult<ConflictAction> {
    let on_duplicate = match &i.on {
        None => None,
        Some(sqlparser::ast::OnInsert::DuplicateKeyUpdate(a)) => Some(a.as_slice()),
        Some(sqlparser::ast::OnInsert::OnConflict(c)) => {
            return Err(SqlError::unsupported(format!("ON CONFLICT {c}")));
        }
        // OnInsert is #[non_exhaustive] upstream
        other => return Err(SqlError::unsupported(format!("ON INSERT clause {other:?}"))),
    };
    if let Some(assigns) = on_duplicate {
        if i.replace_into {
            return Err(SqlError::parse(
                "REPLACE ... ON DUPLICATE KEY UPDATE is not valid",
            ));
        }
        let assigns = assigns
            .iter()
            .map(|a| {
                let (col, e) = assignment(a)?;
                // VALUES(col) -> the incoming row's column (IR marker);
                // only this context accepts it.
                Ok((col, rewrite_values(&e, true)?))
            })
            .collect::<SqlResult<Vec<_>>>()?;
        return Ok(ConflictAction::OnDuplicate(assigns));
    }
    if i.replace_into {
        return Ok(ConflictAction::Replace);
    }
    Ok(ConflictAction::Error)
}

/// One `target = value` assignment (ODKU and INSERT ... SET share it).
fn assignment(a: &sqlparser::ast::Assignment) -> SqlResult<(String, Expr)> {
    let sqlparser::ast::AssignmentTarget::ColumnName(n) = &a.target else {
        return Err(SqlError::unsupported("tuple assignment targets"));
    };
    Ok((object_name(n)?, translate_expr(&a.value)?))
}

/// Column list + row source: the SET form normalizes to a named
/// single-row VALUES list; otherwise the source query must be literal
/// VALUES tuples or a SELECT (INSERT ... SELECT).
fn source_of(i: &sqlparser::ast::Insert) -> SqlResult<(Vec<String>, InsertSource)> {
    let columns = i
        .columns
        .iter()
        .map(object_name)
        .collect::<SqlResult<Vec<String>>>()?;
    if !i.assignments.is_empty() {
        // INSERT ... SET: exactly a one-row named column list.
        if i.source.is_some() {
            return Err(SqlError::parse(
                "INSERT ... SET takes no VALUES/SELECT source",
            ));
        }
        let mut cols = Vec::with_capacity(i.assignments.len());
        let mut row = Vec::with_capacity(i.assignments.len());
        for a in &i.assignments {
            let (col, e) = assignment(a)?;
            reject_values_fn(&e)?;
            cols.push(col);
            row.push(e);
        }
        return Ok((cols, InsertSource::Values(vec![row])));
    }
    let source = i
        .source
        .as_deref()
        .ok_or_else(|| SqlError::parse("INSERT needs VALUES, SELECT or SET"))?;
    if source.with.is_some() {
        return Err(SqlError::unsupported("INSERT with CTE"));
    }
    match source.body.as_ref() {
        SetExpr::Values(values) => {
            let rows = values
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|e| {
                            let e = translate_expr(e)?;
                            reject_values_fn(&e)?;
                            Ok(e)
                        })
                        .collect::<SqlResult<Vec<_>>>()
                })
                .collect::<SqlResult<Vec<Vec<_>>>>()?;
            for row in &rows {
                if row.is_empty() || (!columns.is_empty() && row.len() != columns.len()) {
                    return Err(SqlError::new(
                        ErrorCode::WrongValueCount,
                        "INSERT row arity does not match column list",
                    ));
                }
            }
            Ok((columns, InsertSource::Values(rows)))
        }
        SetExpr::Select(_) => Ok((
            columns,
            InsertSource::Select(Box::new(translate_compound(source)?)),
        )),
        other => Err(SqlError::unsupported(format!(
            "INSERT source other than VALUES/SELECT: {other}"
        ))),
    }
}

/// Rewrite `VALUES(col)` function calls in one already-translated expr.
/// `allow` is true only for ODKU assignments; there the call becomes
/// the incoming-row marker, everywhere else it is a parse error.
fn rewrite_values(e: &Expr, allow: bool) -> SqlResult<Expr> {
    let bad = || SqlError::parse("VALUES() is only allowed in ON DUPLICATE KEY UPDATE assignments");
    Ok(match e {
        Expr::Func { name, args } if name.eq_ignore_ascii_case("values") => {
            if !allow {
                return Err(bad());
            }
            match args.as_slice() {
                [Expr::Col { table: None, name }] => Expr::InsertValues(name.clone()),
                _ => return Err(SqlError::parse("VALUES() takes exactly one column name")),
            }
        }
        Expr::Lit(v) => Expr::Lit(v.clone()),
        Expr::Col { table, name } => Expr::Col {
            table: table.clone(),
            name: name.clone(),
        },
        Expr::Placeholder | Expr::InsertValues(_) => e.clone(),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(rewrite_values(left, allow)?),
            op: *op,
            right: Box::new(rewrite_values(right, allow)?),
        },
        Expr::Not(x) => Expr::Not(Box::new(rewrite_values(x, allow)?)),
        Expr::Neg(x) => Expr::Neg(Box::new(rewrite_values(x, allow)?)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(rewrite_values(expr, allow)?),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(rewrite_values(expr, allow)?),
            list: list
                .iter()
                .map(|x| rewrite_values(x, allow))
                .collect::<SqlResult<Vec<_>>>()?,
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rewrite_values(expr, allow)?),
            low: Box::new(rewrite_values(low, allow)?),
            high: Box::new(rewrite_values(high, allow)?),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(rewrite_values(expr, allow)?),
            pattern: Box::new(rewrite_values(pattern, allow)?),
            negated: *negated,
        },
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => Expr::Case {
            operand: match operand.as_ref() {
                Some(x) => Some(Box::new(rewrite_values(x, allow)?)),
                None => None,
            },
            branches: branches
                .iter()
                .map(|(c, t)| Ok((rewrite_values(c, allow)?, rewrite_values(t, allow)?)))
                .collect::<SqlResult<Vec<_>>>()?,
            else_expr: match else_expr.as_ref() {
                Some(x) => Some(Box::new(rewrite_values(x, allow)?)),
                None => None,
            },
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(rewrite_values(expr, allow)?),
            to: *to,
        },
        Expr::Regexp {
            expr,
            pattern,
            negated,
        } => Expr::Regexp {
            expr: Box::new(rewrite_values(expr, allow)?),
            pattern: Box::new(rewrite_values(pattern, allow)?),
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(rewrite_values(expr, allow)?),
            query: query.clone(),
            negated: *negated,
        },
        Expr::Subquery(q) => Expr::Subquery(q.clone()),
        Expr::Exists { query, negated } => Expr::Exists {
            query: query.clone(),
            negated: *negated,
        },
        // Bind-time node; never present at translation time.
        Expr::Correlated { .. } => e.clone(),
        Expr::Agg {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Agg {
            func: *func,
            arg: match arg.as_ref() {
                Some(x) => Some(Box::new(rewrite_values(x, allow)?)),
                None => None,
            },
            distinct: *distinct,
            sep: sep.clone(),
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|x| rewrite_values(x, allow))
                .collect::<SqlResult<Vec<_>>>()?,
        },
    })
}

/// `VALUES()` outside ODKU assignments (the INSERT row sources): the
/// rewriter with `allow = false` rejects it and otherwise returns the
/// expr unchanged.
fn reject_values_fn(e: &Expr) -> SqlResult<()> {
    rewrite_values(e, false).map(|_| ())
}

#[cfg(test)]
#[path = "translate_dml_tests.rs"]
mod tests;
