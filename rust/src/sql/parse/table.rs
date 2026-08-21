//! FROM-clause translation: tables and INNER JOINs.

use sqlparser::ast::{JoinConstraint, JoinOperator, TableFactor, TableWithJoins};

use crate::sql::parse::ast::{Expr, TableRef};
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::parse::translate::{object_name, translate_expr};

pub(crate) fn translate_table_with_joins(twj: &TableWithJoins) -> SqlResult<TableRef> {
    let mut acc = translate_factor(&twj.relation)?;
    for join in &twj.joins {
        acc = translate_join(acc, join)?;
    }
    Ok(acc)
}

fn translate_factor(f: &TableFactor) -> SqlResult<TableRef> {
    match f {
        TableFactor::Table { name, alias, .. } => Ok(TableRef::Table {
            name: object_name(name)?,
            alias: alias.as_ref().map(|a| a.name.value.clone()),
        }),
        other => Err(SqlError::unsupported(format!(
            "FROM factor {other} (subqueries are not supported)"
        ))),
    }
}

fn translate_join(left: TableRef, join: &sqlparser::ast::Join) -> SqlResult<TableRef> {
    let sqlparser::ast::Join {
        relation,
        join_operator,
        ..
    } = join;
    let (right, on) = match join_operator {
        JoinOperator::Inner(constraint) | JoinOperator::Join(constraint) => {
            (translate_factor(relation)?, constraint_expr(constraint)?)
        }
        other => {
            let _ = other;
            return Err(SqlError::unsupported("only INNER JOIN"));
        }
    };
    Ok(TableRef::Join {
        left: Box::new(left),
        right: Box::new(right),
        on,
    })
}

fn constraint_expr(c: &JoinConstraint) -> SqlResult<Option<Expr>> {
    match c {
        JoinConstraint::On(e) => translate_expr(e).map(Some),
        JoinConstraint::None => Ok(None),
        JoinConstraint::Using(_) | JoinConstraint::Natural => {
            Err(SqlError::unsupported("JOIN ... USING / NATURAL JOIN"))
        }
    }
}
