//! Translation of the sqlparser "special forms" into the plain
//! `Expr::Func` calls the evaluator already dispatches: SUBSTRING /
//! TRIM / POSITION spellings, `INTERVAL n unit` desugaring (both the
//! DATE_ADD argument and the `d +/- INTERVAL ...` operator form) and
//! GROUP_CONCAT's SEPARATOR clause. Keeps `parse/expr.rs` match arms
//! one-liners.

use sqlparser::ast::{
    CeilFloorKind, DateTimeField, FunctionArgumentClause, FunctionArguments, Interval,
    TrimWhereField,
};

use crate::sql::parse::ast::{AggFunc, Expr};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};

use super::expr::{translate_expr, translate_value};

/// `SUBSTRING(s, pos[, len])` in every spelling sqlparser accepts
/// (comma form, `FROM .. FOR ..` form, SUBSTR shorthand): one Func
/// node, one evaluator.
pub(crate) fn translate_substring(
    expr: &sqlparser::ast::Expr,
    from: &Option<Box<sqlparser::ast::Expr>>,
    for_: &Option<Box<sqlparser::ast::Expr>>,
) -> SqlResult<Expr> {
    let mut args = vec![translate_expr(expr)?];
    // No FROM: the whole string from position 1.
    args.push(match from {
        Some(f) => translate_expr(f)?,
        None => Expr::Lit(crate::sql::storage::schema::Value::Int(1)),
    });
    if let Some(f) = for_ {
        args.push(translate_expr(f)?);
    }
    Ok(Expr::Func {
        name: "substring".to_string(),
        args,
    })
}

/// `TRIM(s)` / `TRIM([BOTH|LEADING|TRAILING] [remstr] FROM s)`:
/// plain form stays 1-arg (spaces); the extended form is the 3-arg
/// `(s, remstr, mode-literal)` shape the evaluator splits on.
pub(crate) fn translate_trim(
    trim_where: &Option<TrimWhereField>,
    trim_what: &Option<Box<sqlparser::ast::Expr>>,
    expr: &sqlparser::ast::Expr,
) -> SqlResult<Expr> {
    let s = translate_expr(expr)?;
    if trim_where.is_none() && trim_what.is_none() {
        return Ok(Expr::Func {
            name: "trim".to_string(),
            args: vec![s],
        });
    }
    // `TRIM('x' FROM s)` (no keyword) is BOTH; a keyword without a
    // remstr does not parse under the MySQL dialect, but stay loud.
    let remstr = translate_expr(
        trim_what
            .as_deref()
            .ok_or_else(|| SqlError::parse("TRIM mode without a remstr is not representable"))?,
    )?;
    let mode = match trim_where {
        None => "BOTH",
        Some(TrimWhereField::Both) => "BOTH",
        Some(TrimWhereField::Leading) => "LEADING",
        Some(TrimWhereField::Trailing) => "TRAILING",
    };
    Ok(Expr::Func {
        name: "trim".to_string(),
        args: vec![
            s,
            remstr,
            Expr::Lit(crate::sql::storage::schema::Value::Str(mode.to_string())),
        ],
    })
}

/// `POSITION(substr IN s)` == `LOCATE(substr, s)`.
pub(crate) fn translate_position(
    needle: &sqlparser::ast::Expr,
    haystack: &sqlparser::ast::Expr,
) -> SqlResult<Expr> {
    Ok(Expr::Func {
        name: "locate".to_string(),
        args: vec![translate_expr(needle)?, translate_expr(haystack)?],
    })
}

/// `INTERVAL n unit` -> `(value-expr, unit-literal)`. The units the
/// date math actually implements; anything wider is a loud
/// unsupported, not a silent mis-add. MICROSECOND rides along because
/// the storage clock is microsecond-native.
pub(crate) fn translate_interval(iv: &Interval) -> SqlResult<(Expr, Expr)> {
    let unit = match iv.leading_field.as_ref() {
        Some(DateTimeField::Year | DateTimeField::Years) => "YEAR",
        Some(DateTimeField::Month | DateTimeField::Months) => "MONTH",
        Some(DateTimeField::Day | DateTimeField::Days) => "DAY",
        Some(DateTimeField::Hour | DateTimeField::Hours) => "HOUR",
        Some(DateTimeField::Minute | DateTimeField::Minutes) => "MINUTE",
        Some(DateTimeField::Second | DateTimeField::Seconds) => "SECOND",
        Some(DateTimeField::Microsecond | DateTimeField::Microseconds) => "MICROSECOND",
        other => {
            return Err(SqlError::unsupported(format!(
                "INTERVAL unit {other:?} (v1: YEAR/MONTH/DAY/HOUR/MINUTE/SECOND/MICROSECOND)"
            )))
        }
    };
    if iv.last_field.is_some()
        || iv.leading_precision.is_some()
        || iv.fractional_seconds_precision.is_some()
    {
        return Err(SqlError::unsupported("INTERVAL range/precision forms"));
    }
    Ok((
        translate_expr(&iv.value)?,
        Expr::Lit(crate::sql::storage::schema::Value::Str(unit.to_string())),
    ))
}

/// `d +/- INTERVAL n unit` operator form -> the date_add/date_sub
/// call (interval on either side; MySQL accepts both).
pub(crate) fn translate_interval_binop(
    add: bool,
    left: &sqlparser::ast::Expr,
    right: &sqlparser::ast::Expr,
) -> SqlResult<Expr> {
    let (iv, other) = pick_interval(left, right)?;
    let (n, unit) = translate_interval(iv)?;
    Ok(Expr::Func {
        name: if add { "date_add" } else { "date_sub" }.to_string(),
        args: vec![translate_expr(other)?, n, unit],
    })
}

/// The INTERVAL side and the date side, whichever way round they sit.
fn pick_interval<'a>(
    left: &'a sqlparser::ast::Expr,
    right: &'a sqlparser::ast::Expr,
) -> SqlResult<(&'a Interval, &'a sqlparser::ast::Expr)> {
    match (left, right) {
        (sqlparser::ast::Expr::Interval(iv), other)
        | (other, sqlparser::ast::Expr::Interval(iv)) => Ok((iv, other)),
        _ => Err(SqlError::unsupported("INTERVAL operator form")),
    }
}

/// DATE_ADD/DATE_SUB/ADDDATE/SUBDATE: `[date, INTERVAL n unit]`
/// desugars to the 3-arg `(date, n, unit-literal)` call; the plain
/// `[date, days]` form stays 2-arg (ADDDATE/SUBDATE only).
pub(crate) fn translate_date_add(fargs: &FunctionArguments, name: &str) -> SqlResult<Expr> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr};
    let wrong = || crate::sql::exec::func::wrong_param_count(name);
    let FunctionArguments::List(list) = fargs else {
        return Err(wrong());
    };
    if !list.clauses.is_empty() || list.duplicate_treatment.is_some() {
        return Err(SqlError::unsupported(format!("{name} clauses")));
    }
    let mut args: Vec<Expr> = Vec::new();
    let mut interval: Option<(Expr, Expr)> = None;
    for a in &list.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = a else {
            return Err(SqlError::unsupported("function argument forms"));
        };
        if let sqlparser::ast::Expr::Interval(iv) = e {
            if interval.is_some() {
                return Err(wrong());
            }
            interval = Some(translate_interval(iv)?);
        } else {
            args.push(translate_expr(e)?);
        }
    }
    let args = match (args.len(), interval) {
        (1, Some((n, unit))) => vec![args.remove(0), n, unit],
        // The plain-days ADDDATE/SUBDATE spelling; DATE_ADD/SUBDATE
        // with no INTERVAL is caught by the signature table instead.
        (2, None) if matches!(name, "adddate" | "subdate") => args,
        _ => return Err(wrong()),
    };
    Ok(Expr::Func {
        name: name.to_string(),
        args,
    })
}

/// GROUP_CONCAT: plain args (NULLs skipped at aggregation), DISTINCT
/// reuse, SEPARATOR clause -> node field. Inner ORDER BY is the loud
/// P2 reject (needs in-group sort infrastructure; gap-matrix B/E).
pub(crate) fn translate_group_concat(
    fargs: &FunctionArguments,
    args: Vec<Expr>,
    distinct: bool,
) -> SqlResult<Expr> {
    let mut sep = None;
    if let FunctionArguments::List(list) = fargs {
        for c in &list.clauses {
            match c {
                FunctionArgumentClause::Separator(v) => {
                    sep = Some(match translate_value(v)? {
                        crate::sql::storage::schema::Value::Str(s) => s,
                        other => {
                            return Err(SqlError::parse(format!(
                                "GROUP_CONCAT separator must be a string, got {other:?}"
                            )))
                        }
                    });
                }
                FunctionArgumentClause::OrderBy(_) => {
                    return Err(SqlError::new(
                        ErrorCode::NotSupported,
                        "GROUP_CONCAT ORDER BY (v1: unordered groups)",
                    ))
                }
                other => {
                    return Err(SqlError::unsupported(format!(
                        "GROUP_CONCAT clause {other:?}"
                    )))
                }
            }
        }
    }
    let arg = match args.len() {
        0 => return Err(SqlError::parse("GROUP_CONCAT needs an argument")),
        1 => args.into_iter().next().expect("len checked"),
        // MySQL concatenates each row's comma-separated arguments with
        // the same separator; wrapping in CONCAT keeps that exactly.
        _ => Expr::Func {
            name: "concat".to_string(),
            args,
        },
    };
    Ok(Expr::Agg {
        func: AggFunc::GroupConcat,
        arg: Some(Box::new(arg)),
        distinct,
        sep,
    })
}

/// `CEIL/FLOOR(x)` keyword spellings: sqlparser emits dedicated
/// `Expr::Ceil`/`Expr::Floor` nodes instead of Function calls. The
/// plain form maps onto the same "ceiling"/"floor" Func the
/// evaluator dispatches; the `TO <datetime field>` spelling stays a
/// loud unsupported (interval-scaled ceil/floor is not implemented).
pub(crate) fn translate_ceil_floor(
    up: bool,
    expr: &sqlparser::ast::Expr,
    field: &CeilFloorKind,
) -> SqlResult<Expr> {
    if !matches!(
        field,
        CeilFloorKind::DateTimeField(DateTimeField::NoDateTime)
    ) {
        return Err(SqlError::unsupported("CEIL/FLOOR TO <unit>"));
    }
    Ok(Expr::Func {
        name: if up { "ceiling" } else { "floor" }.to_string(),
        args: vec![translate_expr(expr)?],
    })
}
