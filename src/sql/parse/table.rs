//! FROM-clause translation: tables, derived tables and joins.

use sqlparser::ast::{JoinConstraint, JoinOperator, TableFactor, TableWithJoins};

use crate::sql::parse::ast::{Expr, JoinKind, TableRef};
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
    let (right, kind, on, using) = match join_operator {
        JoinOperator::Inner(constraint) | JoinOperator::Join(constraint) => {
            let (on, using) = constraint_parts(constraint)?;
            (translate_factor(relation)?, JoinKind::Inner, on, using)
        }
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            let (on, using) = constraint_parts(constraint)?;
            (translate_factor(relation)?, JoinKind::Left, on, using)
        }
        JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) => {
            let (on, using) = constraint_parts(constraint)?;
            (translate_factor(relation)?, JoinKind::Right, on, using)
        }
        JoinOperator::FullOuter(constraint) => {
            let (on, using) = constraint_parts(constraint)?;
            (translate_factor(relation)?, JoinKind::Full, on, using)
        }
        JoinOperator::CrossJoin(constraint) => {
            // CROSS JOIN takes no condition; sqlparser may still park a
            // constraint there (non-standard), which we must not lose.
            let (on, using) = constraint_parts(constraint)?;
            (translate_factor(relation)?, JoinKind::Cross, on, using)
        }
        JoinOperator::Semi(_)
        | JoinOperator::LeftSemi(_)
        | JoinOperator::RightSemi(_)
        | JoinOperator::Anti(_)
        | JoinOperator::LeftAnti(_)
        | JoinOperator::RightAnti(_)
        | JoinOperator::StraightJoin(_)
        | JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::AsOf { .. }
        | JoinOperator::ArrayJoin
        | JoinOperator::LeftArrayJoin
        | JoinOperator::InnerArrayJoin => {
            return Err(SqlError::unsupported("SEMI/ANTI/STRAIGHT/APPLY joins"))
        }
    };
    Ok(TableRef::Join {
        left: Box::new(left),
        right: Box::new(right),
        kind,
        on,
        using,
    })
}

/// ON expr and USING column list of one join constraint (ON and USING
/// are mutually exclusive per grammar; NATURAL needs schema knowledge
/// and stays rejected).
fn constraint_parts(c: &JoinConstraint) -> SqlResult<(Option<Expr>, Vec<String>)> {
    match c {
        JoinConstraint::On(e) => Ok((translate_expr(e).map(Some)?, Vec::new())),
        JoinConstraint::None => Ok((None, Vec::new())),
        JoinConstraint::Using(cols) => Ok((
            None,
            cols.iter()
                .map(object_name)
                .collect::<SqlResult<Vec<_>>>()?,
        )),
        JoinConstraint::Natural => Err(SqlError::unsupported("NATURAL JOIN")),
    }
}
