//! Expression translation: sqlparser Expr -> IR Expr.

use sqlparser::ast::{BinaryOperator, Expr as SqlExpr, UnaryOperator};

use crate::sql::parse::ast::{AggFunc, BinOp, Expr};
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::schema::Value;

pub(crate) fn translate_expr(e: &SqlExpr) -> SqlResult<Expr> {
    use SqlExpr as S;
    Ok(match e {
        S::Identifier(id) => Expr::Col {
            table: None,
            name: id.value.clone(),
        },
        S::CompoundIdentifier(parts) => {
            if parts.len() != 2 {
                return Err(SqlError::unsupported(format!("reference {e}")));
            }
            Expr::Col {
                table: Some(parts[0].value.clone()),
                name: parts[1].value.clone(),
            }
        }
        S::Value(v) => match &v.value {
            // Bare `?` is the prepared-statement parameter; anything with
            // a name is rejected so binding stays positional.
            sqlparser::ast::Value::Placeholder(p) if p == "?" => Expr::Placeholder,
            _ => Expr::Lit(translate_value(v)?),
        },
        S::Nested(inner) => translate_expr(inner)?,
        S::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(translate_expr(left)?),
            op: translate_binop(op)?,
            right: Box::new(translate_expr(right)?),
        },
        S::UnaryOp { op, expr } => match op {
            UnaryOperator::Not => Expr::Not(Box::new(translate_expr(expr)?)),
            UnaryOperator::Minus => Expr::Neg(Box::new(translate_expr(expr)?)),
            other => return Err(SqlError::unsupported(format!("unary {other}"))),
        },
        S::IsNull(inner) => Expr::IsNull {
            expr: Box::new(translate_expr(inner)?),
            negated: false,
        },
        S::IsNotNull(inner) => Expr::IsNull {
            expr: Box::new(translate_expr(inner)?),
            negated: true,
        },
        S::InList {
            expr,
            list,
            negated,
        } => Expr::InList {
            expr: Box::new(translate_expr(expr)?),
            list: list
                .iter()
                .map(translate_expr)
                .collect::<SqlResult<Vec<_>>>()?,
            negated: *negated,
        },
        S::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(translate_expr(expr)?),
            low: Box::new(translate_expr(low)?),
            high: Box::new(translate_expr(high)?),
            negated: *negated,
        },
        S::Like {
            negated,
            expr,
            pattern,
            ..
        } => Expr::Like {
            expr: Box::new(translate_expr(expr)?),
            pattern: Box::new(translate_expr(pattern)?),
            negated: *negated,
        },
        S::Function(f) => translate_function(f)?,
        S::IsTrue(_) | S::IsFalse(_) | S::IsNotTrue(_) | S::IsNotFalse(_) => {
            return Err(SqlError::unsupported("IS TRUE / IS FALSE"))
        }
        S::Cast { .. } => return Err(SqlError::unsupported("CAST (storage is typed already)")),
        S::Substring { .. } | S::Trim { .. } | S::Position { .. } | S::Overlay { .. } => {
            return Err(SqlError::unsupported(
                "string special forms; use the function form",
            ))
        }
        other => return Err(SqlError::unsupported(format!("{other}"))),
    })
}

fn translate_binop(op: &BinaryOperator) -> SqlResult<BinOp> {
    use BinaryOperator as B;
    Ok(match op {
        B::Plus => BinOp::Add,
        B::Minus => BinOp::Sub,
        B::Multiply => BinOp::Mul,
        B::Divide => BinOp::Div,
        B::Modulo => BinOp::Mod,
        B::Gt => BinOp::Gt,
        B::Lt => BinOp::Lt,
        B::GtEq => BinOp::GtEq,
        B::LtEq => BinOp::LtEq,
        B::Eq => BinOp::Eq,
        B::NotEq => BinOp::NotEq,
        B::And => BinOp::And,
        B::Or => BinOp::Or,
        other => return Err(SqlError::unsupported(format!("operator {other}"))),
    })
}

fn translate_function(f: &sqlparser::ast::Function) -> SqlResult<Expr> {
    use sqlparser::ast::{DuplicateTreatment, FunctionArgExpr, FunctionArguments};
    let name = f.name.to_string().to_lowercase();
    let (args, distinct, wildcard) = match &f.args {
        FunctionArguments::None => (Vec::new(), false, false),
        FunctionArguments::List(list) => {
            let mut args = Vec::new();
            let mut wildcard = false;
            for a in &list.args {
                match a {
                    sqlparser::ast::FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        args.push(translate_expr(e)?)
                    }
                    sqlparser::ast::FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {
                        wildcard = true
                    }
                    _ => return Err(SqlError::unsupported("function argument forms")),
                }
            }
            (
                args,
                matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct)),
                wildcard,
            )
        }
        FunctionArguments::Subquery(_) => {
            return Err(SqlError::unsupported("subquery function arguments"))
        }
    };
    if f.filter.is_some() {
        return Err(SqlError::unsupported("FILTER clauses"));
    }
    let agg = match name.as_str() {
        "count" if wildcard => {
            if !args.is_empty() || distinct {
                return Err(SqlError::parse("COUNT(*) takes no arguments"));
            }
            return Ok(Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                distinct: false,
            });
        }
        "count" => Some(AggFunc::Count),
        "sum" => Some(AggFunc::Sum),
        "avg" => Some(AggFunc::Avg),
        "min" => Some(AggFunc::Min),
        "max" => Some(AggFunc::Max),
        _ => None,
    };
    if let Some(func) = agg {
        if f.over.is_some() {
            return Err(SqlError::unsupported("window functions"));
        }
        let arg = match args.len() {
            0 if matches!(func, AggFunc::Count) => None,
            1 => args.into_iter().next(),
            _ => return Err(SqlError::unsupported(format!("{name} arity"))),
        };
        return Ok(Expr::Agg {
            func,
            arg: arg.map(Box::new),
            distinct,
        });
    }
    Ok(Expr::Func { name, args })
}

pub(crate) fn translate_value(v: &sqlparser::ast::ValueWithSpan) -> SqlResult<Value> {
    use sqlparser::ast::Value as SqlValue;
    Ok(match &v.value {
        SqlValue::Number(n, _) => {
            if let Ok(i) = n.parse::<i64>() {
                Value::Int(i)
            } else {
                n.parse::<f64>()
                    .map(Value::Double)
                    .map_err(|_| SqlError::parse(format!("bad number {n}")))?
            }
        }
        SqlValue::SingleQuotedString(s) | SqlValue::DoubleQuotedString(s) => Value::Str(s.clone()),
        SqlValue::NationalStringLiteral(s) => Value::Str(s.clone()),
        SqlValue::EscapedStringLiteral(s) => Value::Str(s.clone()),
        SqlValue::Boolean(b) => Value::Bool(*b),
        SqlValue::Null => Value::Null,
        SqlValue::Placeholder(s) => return Err(SqlError::parse(format!("named placeholder {s}"))),
        other => return Err(SqlError::unsupported(format!("literal {other}"))),
    })
}
