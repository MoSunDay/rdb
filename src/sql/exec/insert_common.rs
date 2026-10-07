//! Row construction shared by every INSERT source (VALUES tuples,
//! INSERT ... SELECT, and the normalized INSERT ... SET form): expand
//! the given column list (or positional order), evaluate/coerce cell
//! values to the column types and enforce NOT NULL.
//!
//! Split out of `exec::write` (M2) so the plain, ODKU and REPLACE
//! paths all build rows through ONE function; the AUTO_INCREMENT slot
//! survives as NULL here -- its NOT NULL check defers to the allocator
//! (`exec::sequence`), which rewrites the slot right after.

use crate::sql::exec::expr::{coerce, eval, SingleTableScope};
use crate::sql::parse::ast::Expr;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{TableSchema, Value};

/// MySQL ER 1054: unknown column in a field list.
pub(crate) fn bad_field(col: &str) -> SqlError {
    SqlError::new(
        ErrorCode::BadField,
        format!("unknown column '{col}' in 'field list'"),
    )
}

/// Evaluate one INSERT VALUES tuple into plain cell values: column
/// references reject (no row context exists yet). Coercion and the
/// column-list expansion happen once, later, in [`build_row_values`]
/// (shared with the INSERT ... SELECT source).
pub(crate) fn eval_cells(schema: &TableSchema, exprs: &[Expr]) -> SqlResult<Vec<Value>> {
    for e in exprs {
        reject_col_refs(e)?;
    }
    let names: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
    let scope = SingleTableScope { columns: &names };
    exprs.iter().map(|e| eval(e, &scope, &[])).collect()
}

/// Build one full-width row from already-evaluated cell values (the
/// INSERT ... SELECT source and the VALUES path converge here):
/// positional placement when no column list was given, else by name;
/// coerce to the column types; enforce NOT NULL except on the
/// AUTO_INCREMENT slot (`ai`), which may stay NULL for one statement
/// tick until the allocator rewrites it.
pub(crate) fn build_row_values(
    schema: &TableSchema,
    columns: &[String],
    cells: Vec<Value>,
    ai: Option<usize>,
) -> SqlResult<Vec<Value>> {
    let mut slots: Vec<Option<Value>> = vec![None; schema.columns.len()];
    if columns.is_empty() {
        // Positional form: the tuple must name every column in order.
        if cells.len() != schema.columns.len() {
            return Err(SqlError::new(
                ErrorCode::WrongValueCount,
                format!(
                    "row has {} values, table '{}' has {} columns",
                    cells.len(),
                    schema.name,
                    schema.columns.len()
                ),
            ));
        }
        for (i, v) in cells.into_iter().enumerate() {
            slots[i] = Some(v);
        }
    } else {
        if cells.len() != columns.len() {
            return Err(SqlError::new(
                ErrorCode::WrongValueCount,
                "column count doesn't match value count",
            ));
        }
        for (col, v) in columns.iter().zip(cells) {
            let idx = schema.column_index(col).ok_or_else(|| bad_field(col))?;
            if slots[idx].is_some() {
                return Err(SqlError::new(
                    ErrorCode::Parse,
                    format!("column '{col}' specified twice"),
                ));
            }
            slots[idx] = Some(v);
        }
    }
    let mut out = Vec::with_capacity(schema.columns.len());
    for (i, col) in schema.columns.iter().enumerate() {
        let v = coerce(slots[i].take().unwrap_or(Value::Null), col.sql_type)?;
        if Some(i) != ai {
            check_not_null(&v, &col.name, col.nullable)?;
        }
        out.push(v);
    }
    Ok(out)
}

pub(crate) fn check_not_null(v: &Value, name: &str, nullable: bool) -> SqlResult<()> {
    if matches!(v, Value::Null) && !nullable {
        return Err(SqlError::new(
            ErrorCode::BadNull,
            format!("column '{name}' cannot be null"),
        ));
    }
    Ok(())
}

/// Column references are not allowed in VALUES (no row exists yet).
pub(crate) fn reject_col_refs(e: &Expr) -> SqlResult<()> {
    match e {
        Expr::Col { name, .. } => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("column '{name}' is not allowed in VALUES"),
        )),
        Expr::Lit(_) | Expr::Placeholder | Expr::Agg { arg: None, .. } => Ok(()),
        // VALUES(col) markers only exist in ODKU assignments, which
        // never pass through here; keep the arm explicit anyway.
        Expr::InsertValues(col) => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("VALUES({col}) is not allowed in this context"),
        )),
        Expr::Subquery(_)
        | Expr::InSubquery { .. }
        | Expr::Exists { .. }
        | Expr::Correlated { .. } => Err(SqlError::new(
            ErrorCode::NotSupported,
            "subqueries are not allowed in VALUES",
        )),
        Expr::Agg { arg: Some(a), .. } => reject_col_refs(a),
        Expr::Func { args, .. } => {
            for a in args {
                reject_col_refs(a)?;
            }
            Ok(())
        }
        Expr::BinaryOp { left, right, .. } => {
            reject_col_refs(left)?;
            reject_col_refs(right)
        }
        Expr::Not(x) | Expr::Neg(x) => reject_col_refs(x),
        Expr::IsNull { expr, .. } => reject_col_refs(expr),
        Expr::InList { expr, list, .. } => {
            reject_col_refs(expr)?;
            for i in list {
                reject_col_refs(i)?;
            }
            Ok(())
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            reject_col_refs(expr)?;
            reject_col_refs(low)?;
            reject_col_refs(high)
        }
        Expr::Like { expr, pattern, .. } => {
            reject_col_refs(expr)?;
            reject_col_refs(pattern)
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            if let Some(o) = operand {
                reject_col_refs(o)?;
            }
            for (c, t) in branches {
                reject_col_refs(c)?;
                reject_col_refs(t)?;
            }
            match else_expr {
                Some(e) => reject_col_refs(e),
                None => Ok(()),
            }
        }
        Expr::Cast { expr, .. } => reject_col_refs(expr),
        Expr::Regexp { expr, pattern, .. } => {
            reject_col_refs(expr)?;
            reject_col_refs(pattern)
        }
    }
}
/// Substitute `VALUES(col)` markers with the incoming row's value; the
/// result evaluates like any UPDATE assignment against the existing row.
pub(crate) fn subst_values(e: &Expr, schema: &TableSchema, incoming: &[Value]) -> SqlResult<Expr> {
    Ok(match e {
        Expr::InsertValues(col) => {
            let idx = schema.column_index(col).ok_or_else(|| bad_field(col))?;
            Expr::Lit(incoming[idx].clone())
        }
        Expr::Lit(v) => Expr::Lit(v.clone()),
        Expr::Col { table, name } => Expr::Col {
            table: table.clone(),
            name: name.clone(),
        },
        Expr::Placeholder | Expr::Subquery(_) | Expr::Agg { arg: None, .. } => e.clone(),
        Expr::Exists { query, negated } => Expr::Exists {
            query: query.clone(),
            negated: *negated,
        },
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            query: query.clone(),
            negated: *negated,
        },
        // Bind-time node; never present on the upsert path.
        Expr::Correlated { .. } => e.clone(),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(subst_values(left, schema, incoming)?),
            op: *op,
            right: Box::new(subst_values(right, schema, incoming)?),
        },
        Expr::Not(x) => Expr::Not(Box::new(subst_values(x, schema, incoming)?)),
        Expr::Neg(x) => Expr::Neg(Box::new(subst_values(x, schema, incoming)?)),
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            negated: *negated,
        },
        Expr::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            list: list
                .iter()
                .map(|x| subst_values(x, schema, incoming))
                .collect::<SqlResult<Vec<_>>>()?,
            negated: *negated,
        },
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            low: Box::new(subst_values(low, schema, incoming)?),
            high: Box::new(subst_values(high, schema, incoming)?),
            negated: *negated,
        },
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Expr::Like {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            pattern: Box::new(subst_values(pattern, schema, incoming)?),
            negated: *negated,
        },
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => Expr::Case {
            operand: match operand.as_ref() {
                Some(x) => Some(Box::new(subst_values(x, schema, incoming)?)),
                None => None,
            },
            branches: branches
                .iter()
                .map(|(c, t)| {
                    Ok((
                        subst_values(c, schema, incoming)?,
                        subst_values(t, schema, incoming)?,
                    ))
                })
                .collect::<SqlResult<Vec<_>>>()?,
            else_expr: match else_expr.as_ref() {
                Some(x) => Some(Box::new(subst_values(x, schema, incoming)?)),
                None => None,
            },
        },
        Expr::Cast { expr, to } => Expr::Cast {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            to: *to,
        },
        Expr::Regexp {
            expr,
            pattern,
            negated,
        } => Expr::Regexp {
            expr: Box::new(subst_values(expr, schema, incoming)?),
            pattern: Box::new(subst_values(pattern, schema, incoming)?),
            negated: *negated,
        },
        Expr::Agg {
            func,
            arg,
            distinct,
            sep,
        } => Expr::Agg {
            func: *func,
            arg: match arg.as_ref() {
                Some(x) => Some(Box::new(subst_values(x, schema, incoming)?)),
                None => None,
            },
            distinct: *distinct,
            sep: sep.clone(),
        },
        Expr::Func { name, args } => Expr::Func {
            name: name.clone(),
            args: args
                .iter()
                .map(|x| subst_values(x, schema, incoming))
                .collect::<SqlResult<Vec<_>>>()?,
        },
    })
}
