//! Expression translation: sqlparser Expr -> IR Expr.

use sqlparser::ast::{BinaryOperator, CastKind, Expr as SqlExpr, UnaryOperator};

use crate::sql::parse::ast::{AggFunc, BinOp, CastSpec, Expr};
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::schema::{Value, MAX_DECIMAL_SCALE};

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
        // DATE '...' / DATETIME '...' / TIMESTAMP '...' literals ride as
        // plain strings; the engine coerces on use (coerce/cmp parse).
        S::TypedString(ts) => match &ts.data_type {
            sqlparser::ast::DataType::Date
            | sqlparser::ast::DataType::Datetime(_)
            | sqlparser::ast::DataType::Timestamp(_, _) => {
                Expr::Lit(Value::Str(ts.value.clone().into_string().ok_or_else(
                    || SqlError::parse("typed literal must be a string"),
                )?))
            }
            other => {
                return Err(SqlError::unsupported(format!(
                    "typed string literal {other} (v1: DATE/DATETIME/TIMESTAMP)"
                )))
            }
        },
        S::Nested(inner) => translate_expr(inner)?,
        S::Subquery(inner) => Expr::Subquery(Box::new(
            crate::sql::parse::query::translate_compound(inner)?,
        )),
        S::Exists { subquery, negated } => Expr::Exists {
            query: Box::new(crate::sql::parse::query::translate_compound(subquery)?),
            negated: *negated,
        },
        S::BinaryOp { left, op, right } => {
            // `d +/- INTERVAL n unit` (either side) desugars to the
            // date_add/date_sub function call before the operator
            // table ever sees a temporal operand.
            if matches!(op, BinaryOperator::Plus | BinaryOperator::Minus)
                && (matches!(**left, S::Interval(_)) || matches!(**right, S::Interval(_)))
            {
                return crate::sql::parse::func_forms::translate_interval_binop(
                    matches!(op, BinaryOperator::Plus),
                    left,
                    right,
                );
            }
            Expr::BinaryOp {
                left: Box::new(translate_expr(left)?),
                op: translate_binop(op)?,
                right: Box::new(translate_expr(right)?),
            }
        }
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
        S::InSubquery {
            expr,
            subquery,
            negated,
        } => Expr::InSubquery {
            expr: Box::new(translate_expr(expr)?),
            query: Box::new(crate::sql::parse::query::translate_compound(subquery)?),
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
        // CASE, both forms (operand present = simple CASE).
        S::Case {
            operand,
            conditions,
            else_result,
            ..
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| translate_expr(o).map(Box::new))
                .transpose()?,
            branches: conditions
                .iter()
                .map(|w| Ok((translate_expr(&w.condition)?, translate_expr(&w.result)?)))
                .collect::<SqlResult<Vec<_>>>()?,
            else_expr: else_result
                .as_ref()
                .map(|e| translate_expr(e).map(Box::new))
                .transpose()?,
        },
        // CAST(x AS type): plain CAST only (TRY_/SAFE_CAST/`::` reject).
        S::Cast {
            kind: CastKind::Cast,
            expr,
            data_type,
            ..
        } => Expr::Cast {
            expr: Box::new(translate_expr(expr)?),
            to: translate_cast_spec(data_type)?,
        },
        S::Cast { kind, .. } => return Err(SqlError::unsupported(format!("cast kind {kind:?}"))),
        // CONVERT(x, type) shares the CAST target narrowing;
        // CONVERT(x USING cs) accepts the identity charsets only
        // (storage strings are utf8 already).
        S::Convert {
            is_try: false,
            expr,
            data_type: Some(data_type),
            charset: None,
            target_before_value: false,
            styles,
            ..
        } if styles.is_empty() => Expr::Cast {
            expr: Box::new(translate_expr(expr)?),
            to: translate_cast_spec(data_type)?,
        },
        S::Convert {
            is_try: false,
            expr,
            data_type: None,
            charset: Some(cs),
            target_before_value: false,
            styles,
            ..
        } if styles.is_empty() => {
            let cs = cs.to_string().to_lowercase();
            match cs.as_str() {
                "utf8" | "utf8mb4" => Expr::Cast {
                    expr: Box::new(translate_expr(expr)?),
                    to: CastSpec::Char(None),
                },
                other => {
                    return Err(SqlError::unsupported(format!(
                        "CONVERT ... USING {other} (v1: utf8/utf8mb4 only)"
                    )))
                }
            }
        }
        S::Convert { .. } => {
            return Err(SqlError::unsupported(
                "CONVERT forms (v1: CONVERT(x, type) / CONVERT(x USING utf8)",
            ))
        }
        // REGEXP / RLIKE and their NOT forms (same semantics).
        S::RLike {
            negated,
            expr,
            pattern,
            ..
        } => Expr::Regexp {
            expr: Box::new(translate_expr(expr)?),
            pattern: Box::new(translate_expr(pattern)?),
            negated: *negated,
        },
        // SUBSTRING in every spelling; TRIM plain + BOTH/LEADING/
        // TRAILING remstr forms; POSITION(x IN s) == LOCATE(x, s).
        S::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            crate::sql::parse::func_forms::translate_substring(expr, substring_from, substring_for)?
        }
        S::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters: None,
        } => crate::sql::parse::func_forms::translate_trim(trim_where, trim_what, expr)?,
        S::Position { expr, r#in } => {
            crate::sql::parse::func_forms::translate_position(expr, r#in)?
        }
        S::Trim { .. } | S::Overlay { .. } => {
            return Err(SqlError::unsupported(
                "string special forms; use the function form",
            ))
        }
        S::Ceil { expr, field } => {
            crate::sql::parse::func_forms::translate_ceil_floor(true, expr, field)?
        }
        S::Floor { expr, field } => {
            crate::sql::parse::func_forms::translate_ceil_floor(false, expr, field)?
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
        // `<=>` (NULL-safe equality) and `XOR` (three-valued logical).
        B::Spaceship => BinOp::NullSafeEq,
        B::Xor => BinOp::LogicalXor,
        // Bitwise `& | ^` and shifts. sqlparser tags `<<`/`>>` with the
        // PG-prefixed variants even under the MySQL dialect (probed).
        B::BitwiseAnd => BinOp::BitAnd,
        B::BitwiseOr => BinOp::BitOr,
        B::BitwiseXor => BinOp::BitXor,
        B::PGBitwiseShiftLeft => BinOp::Shl,
        B::PGBitwiseShiftRight => BinOp::Shr,
        other => return Err(SqlError::unsupported(format!("operator {other}"))),
    })
}

/// Narrow a sqlparser CAST/CONVERT target to the MySQL subset the
/// storage layer can express (SIGNED/UNSIGNED/CHAR(n)/DECIMAL(p,s));
/// anything wider (BINARY, DATE, DOUBLE, ...) is a loud unsupported.
fn translate_cast_spec(dt: &sqlparser::ast::DataType) -> SqlResult<CastSpec> {
    use sqlparser::ast::{CharacterLength, DataType as D, ExactNumberInfo};
    let bad = || {
        SqlError::unsupported(format!(
            "cast target {dt} (v1: SIGNED/UNSIGNED/CHAR/DECIMAL)"
        ))
    };
    Ok(match dt {
        D::Signed => CastSpec::Signed,
        D::Unsigned => CastSpec::Unsigned,
        D::Char(None) => CastSpec::Char(None),
        D::Char(Some(CharacterLength::IntegerLength { length, unit: None })) => {
            CastSpec::Char(Some(u32::try_from(*length).map_err(|_| bad())?))
        }
        D::Char(Some(_)) => return Err(bad()), // MAX / CHAR(n CHARACTERS) forms
        D::Decimal(info) => {
            let (precision, scale) = match info {
                ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
                ExactNumberInfo::Precision(p) => (*p, 0),
                // MySQL's bare DECIMAL defaults to (10, 0).
                ExactNumberInfo::None => (10, 0),
            };
            let (p, s) = (
                u8::try_from(precision).map_err(|_| bad())?,
                u8::try_from(scale).map_err(|_| bad())?,
            );
            if p == 0 || p > MAX_DECIMAL_SCALE || s > p {
                return Err(SqlError::unsupported(format!(
                    "cast target {dt} (precision 1..={MAX_DECIMAL_SCALE}, scale <= precision)"
                )));
            }
            CastSpec::Decimal {
                precision: p,
                scale: s,
            }
        }
        _ => return Err(bad()),
    })
}

fn translate_function(f: &sqlparser::ast::Function) -> SqlResult<Expr> {
    use sqlparser::ast::{DuplicateTreatment, FunctionArgExpr, FunctionArguments};
    let name = f.name.to_string().to_lowercase();
    // The date-arithmetic family: the second argument is an INTERVAL
    // special form, so it desugars before the generic argument loop.
    if matches!(
        name.as_str(),
        "date_add" | "date_sub" | "adddate" | "subdate"
    ) {
        return crate::sql::parse::func_forms::translate_date_add(&f.args, &name);
    }
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
                sep: None,
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
            sep: None,
        });
    }
    // GROUP_CONCAT: aggregate with the SEPARATOR clause riding on the
    // node; inner ORDER BY is the loud P2 reject.
    if name == "group_concat" {
        if f.over.is_some() {
            return Err(SqlError::unsupported("window functions"));
        }
        return crate::sql::parse::func_forms::translate_group_concat(&f.args, args, distinct);
    }
    // Scalar functions: the signature table rejects bad arity here
    // (prepare time); unknown names still flow to the evaluator's
    // "unknown function" error.
    if let Some(res) = crate::sql::parse::func_sig::check_signature(&name, args.len()) {
        res?;
    }
    Ok(Expr::Func { name, args })
}

/// Exact decimal literal of a plain `int.frac` number token: the digits
/// fold into an i128 mantissa one place at a time -- f64 is never
/// entered, so `0.1` stays exactly 1/10. Exponent spellings and anything
/// past 38 significant digits (the i128 digit budget) fall back to
/// Double (None), MySQL's own treatment of over-wide literals.
fn exact_number_literal(n: &str) -> Option<Value> {
    if !n.contains('.') || n.contains(['e', 'E']) {
        return None;
    }
    let (int_part, frac_part) = n.split_once('.')?;
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let scale = frac_part.len();
    if scale > usize::from(MAX_DECIMAL_SCALE) {
        return None;
    }
    // Leading zeros carry no precision; the fold below cannot overflow
    // once the significant digit count fits the mantissa budget.
    let significant = format!("{int_part}{frac_part}")
        .trim_start_matches('0')
        .len();
    if significant > usize::from(MAX_DECIMAL_SCALE) {
        return None;
    }
    let mut mantissa: i128 = 0;
    for b in int_part.bytes().chain(frac_part.bytes()) {
        mantissa = mantissa
            .checked_mul(10)?
            .checked_add(i128::from(b - b'0'))?;
    }
    Some(Value::Decimal(mantissa, scale as u8))
}

pub(crate) fn translate_value(v: &sqlparser::ast::ValueWithSpan) -> SqlResult<Value> {
    use sqlparser::ast::Value as SqlValue;
    Ok(match &v.value {
        SqlValue::Number(n, _) => {
            if let Ok(i) = n.parse::<i64>() {
                Value::Int(i)
            } else if let Some(v) = exact_number_literal(n) {
                v
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
