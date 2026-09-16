//! Expression evaluation over decoded rows.
//!
//! Values are [`schema::Value`]s; rows are schema-ordered slices. Column
//! references resolve by (optional table, name) against the query's scope.

use crate::sql::parse::ast::{BinOp, Expr};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{format_decimal, SqlType, Value};
use crate::sql::temporal::{self, MICROS_PER_DAY};

use super::expr_decimal::{arith_decimal, decimal_out_of_range, decimal_to_f64, fit_column, pow10};

/// Resolve `table.name` -> column index; tables must be unambiguous.
pub trait ColumnScope {
    /// Returns Some(index into the row slice).
    fn resolve(&self, table: Option<&str>, name: &str) -> Option<usize>;
}

/// Plain single-table scope: matches any table qualifier (validated later).
pub struct SingleTableScope<'a> {
    pub columns: &'a [String],
}

impl ColumnScope for SingleTableScope<'_> {
    fn resolve(&self, table: Option<&str>, name: &str) -> Option<usize> {
        // A qualified reference against a single-table scope is accepted only
        // when the qualifier matches nothing we know -> reject upstream; here
        // we simply ignore the qualifier (executor validates aliases).
        let _ = table;
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
    }
}

/// Evaluate `e` against `row` (schema-ordered values).
pub fn eval<S: ColumnScope>(e: &Expr, scope: &S, row: &[Value]) -> SqlResult<Value> {
    match e {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Placeholder => Err(SqlError::new(
            ErrorCode::NotSupported,
            "unbound placeholder".to_string(),
        )),
        // Subqueries are pre-materialized by `exec::subquery`; one
        // reaching eval means the rewrite was skipped.
        Expr::Subquery(_) | Expr::InSubquery { .. } => Err(SqlError::new(
            ErrorCode::NotSupported,
            "internal: unrewritten subquery reached evaluation",
        )),
        Expr::Col { table, name } => {
            let idx = scope.resolve(table.as_deref(), name).ok_or_else(|| {
                SqlError::new(ErrorCode::BadField, format!("unknown column '{name}'"))
            })?;
            row.get(idx).cloned().ok_or_else(|| {
                SqlError::new(ErrorCode::BadField, format!("column '{name}' out of range"))
            })
        }
        Expr::BinaryOp { left, op, right } => {
            let l = eval(left, scope, row)?;
            let r = eval(right, scope, row)?;
            eval_binop(op, &l, &r)
        }
        Expr::Not(inner) => {
            let v = eval(inner, scope, row)?;
            Ok(match truthy_as_tristate(&v)? {
                Some(b) => Value::Bool(!b),
                None => Value::Null,
            })
        }
        Expr::Neg(inner) => {
            let v = eval(inner, scope, row)?;
            match v {
                Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
                Value::Double(d) => Ok(Value::Double(-d)),
                Value::Decimal(m, s) => Ok(Value::Decimal(m.wrapping_neg(), s)),
                Value::Null => Ok(Value::Null),
                other => Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("cannot negate {other:?}"),
                )),
            }
        }
        Expr::IsNull { expr, negated } => {
            let v = eval(expr, scope, row)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Bool(if *negated { !is_null } else { is_null }))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let v = eval(expr, scope, row)?;
            let mut items = Vec::with_capacity(list.len());
            for item in list {
                items.push(eval(item, scope, row)?);
            }
            if matches!(v, Value::Null) {
                return Ok(Value::Null);
            }
            // Equality short-circuits first; a NULL member only makes the
            // predicate unknown when no member compared equal (three-valued IN).
            let mut saw_null = false;
            for item in &items {
                if matches!(item, Value::Null) {
                    saw_null = true;
                } else if eq_values(&v, item)? {
                    return Ok(Value::Bool(!*negated));
                }
            }
            if saw_null {
                return Ok(Value::Null);
            }
            Ok(Value::Bool(*negated))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval(expr, scope, row)?;
            let lo = eval(low, scope, row)?;
            let hi = eval(high, scope, row)?;
            if matches!(v, Value::Null) || matches!(lo, Value::Null) || matches!(hi, Value::Null) {
                return Ok(Value::Null);
            }
            let ge_lo = cmp_values(&v, &lo)?.is_ge();
            let le_hi = cmp_values(&v, &hi)?.is_le();
            let inside = ge_lo && le_hi;
            Ok(Value::Bool(if *negated { !inside } else { inside }))
        }
        Expr::Like {
            expr,
            pattern,
            negated,
        } => {
            let v = eval(expr, scope, row)?;
            let p = eval(pattern, scope, row)?;
            let (Value::Str(s), Value::Str(pat)) = (&v, &p) else {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    "LIKE requires string operands".to_string(),
                ));
            };
            let matched = like_match(s, pat);
            Ok(Value::Bool(if *negated { !matched } else { matched }))
        }
        Expr::Agg { .. } => Err(SqlError::new(
            ErrorCode::NotSupported,
            "aggregate used outside aggregation".to_string(),
        )),
        Expr::Func { name, args } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval(a, scope, row)?);
            }
            eval_func(name, &vals)
        }
    }
}

/// SQL three-valued truthiness: NULL -> error context handled by callers.
pub fn truthy(v: &Value) -> SqlResult<bool> {
    match v {
        Value::Null => Ok(false),
        Value::Bool(b) => Ok(*b),
        Value::Int(i) => Ok(*i != 0),
        Value::Double(d) => Ok(*d != 0.0),
        Value::Decimal(m, _) => Ok(*m != 0),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("{other:?} is not a boolean"),
        )),
    }
}

fn eval_binop(op: &BinOp, l: &Value, r: &Value) -> SqlResult<Value> {
    use BinOp::*;
    match op {
        And => {
            if truthy_as_tristate(l)? == Some(false) || truthy_as_tristate(r)? == Some(false) {
                return Ok(Value::Bool(false));
            }
            if truthy_as_tristate(l)? == Some(true) && truthy_as_tristate(r)? == Some(true) {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Null)
        }
        Or => {
            if truthy_as_tristate(l)? == Some(true) || truthy_as_tristate(r)? == Some(true) {
                return Ok(Value::Bool(true));
            }
            if truthy_as_tristate(l)? == Some(false) && truthy_as_tristate(r)? == Some(false) {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Null)
        }
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                return Ok(Value::Null);
            }
            let c = cmp_values(l, r)?;
            let b = match op {
                Eq => c.is_eq(),
                NotEq => c.is_ne(),
                Lt => c.is_lt(),
                LtEq => c.is_le(),
                Gt => c.is_gt(),
                GtEq => c.is_ge(),
                _ => unreachable!(),
            };
            Ok(Value::Bool(b))
        }
        Add | Sub | Mul | Div | Mod => {
            if matches!(l, Value::Null) || matches!(r, Value::Null) {
                return Ok(Value::Null);
            }
            arith(op, l, r)
        }
    }
}

fn truthy_as_tristate(v: &Value) -> SqlResult<Option<bool>> {
    match v {
        Value::Null => Ok(None),
        other => Ok(Some(truthy(other)?)),
    }
}

fn arith(op: &BinOp, l: &Value, r: &Value) -> SqlResult<Value> {
    use BinOp::*;
    // No date arithmetic in v1 (no interval type); say so explicitly
    // instead of leaking the raw debug pairing below.
    if matches!(l, Value::Date(_) | Value::DateTime(_))
        || matches!(r, Value::Date(_) | Value::DateTime(_))
    {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            "DATE/DATETIME values do not support arithmetic".to_string(),
        ));
    }
    // Decimal arithmetic stays exact: operands align to the coarser
    // scale, add/sub/mul ride checked i128 ops (mul adds the scales),
    // and div carries scale+4 with half-away rounding (expr_decimal).
    // A Double operand still wins (coarsened double mode below).
    let decimal_mode = (matches!(l, Value::Decimal(..)) || matches!(r, Value::Decimal(..)))
        && !matches!(l, Value::Double(_))
        && !matches!(r, Value::Double(_));
    if decimal_mode {
        return arith_decimal(op, l, r);
    }
    // Integer arithmetic stays integer unless an operand is a double.
    let double_mode = matches!(l, Value::Double(_)) || matches!(r, Value::Double(_));
    if double_mode {
        let a = as_double(l)?;
        let b = as_double(r)?;
        let v = match op {
            Add => a + b,
            Sub => a - b,
            Mul => a * b,
            Div => {
                if b == 0.0 {
                    return Ok(Value::Null);
                }
                a / b
            }
            Mod => {
                if b == 0.0 {
                    return Ok(Value::Null);
                }
                a % b
            }
            _ => unreachable!(),
        };
        return Ok(Value::Double(v));
    }
    let (Value::Int(a), Value::Int(b)) = (l, r) else {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("arithmetic on {l:?} and {r:?}"),
        ));
    };
    Ok(match op {
        Add => Value::Int(a.wrapping_add(*b)),
        Sub => Value::Int(a.wrapping_sub(*b)),
        Mul => Value::Int(a.wrapping_mul(*b)),
        Div => {
            if *b == 0 {
                return Ok(Value::Null);
            }
            // wrapping, like Add/Sub/Mul above: MIN / -1 must not panic.
            Value::Int(a.wrapping_div(*b))
        }
        Mod => {
            if *b == 0 {
                return Ok(Value::Null);
            }
            Value::Int(a.wrapping_rem(*b))
        }
        _ => unreachable!(),
    })
}

fn as_double(v: &Value) -> SqlResult<f64> {
    match v {
        Value::Int(i) => Ok(*i as f64),
        Value::Double(d) => Ok(*d),
        Value::Decimal(m, s) => Ok(decimal_to_f64(*m, *s)),
        other => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("{other:?} is not numeric"),
        )),
    }
}

pub fn cmp_values(l: &Value, r: &Value) -> SqlResult<std::cmp::Ordering> {
    use std::cmp::Ordering;
    use Value::*;
    Ok(match (l, r) {
        (Int(a), Int(b)) => a.cmp(b),
        (Double(a), Double(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Int(a), Double(b)) => (*a as f64).partial_cmp(b).unwrap_or(Ordering::Equal),
        (Double(a), Int(b)) => a.partial_cmp(&(*b as f64)).unwrap_or(Ordering::Equal),
        // Decimal comparison: same-scale mantissae compare directly;
        // cross-scale and Decimal-vs-Int align exactly in i128 (an
        // alignment overflow is a loud error, never a wrong answer).
        (Decimal(a, sa), Decimal(b, sb)) => {
            let (lo, hi) = if sa <= sb { (*a, *b) } else { (*b, *a) };
            let lift = 10i128
                .checked_pow(u32::from(sa.abs_diff(*sb)))
                .ok_or_else(|| cmp_unsupported(l, r))?;
            let lifted = lo.checked_mul(lift).ok_or_else(|| cmp_unsupported(l, r))?;
            if sa <= sb {
                lifted.cmp(&hi)
            } else {
                hi.cmp(&lifted)
            }
        }
        // Decimal vs Int is exact: the integer lifts to the decimal's
        // scale (an i64 never overflows a x10^38 lift only at the very
        // extremes -- loud error there).
        (Decimal(a, sa), Int(b)) => match i128::from(*b).checked_mul(pow10(*sa)) {
            Some(lifted) => a.cmp(&lifted),
            None => return Err(cmp_unsupported(l, r)),
        },
        (Int(a), Decimal(b, sb)) => match i128::from(*a).checked_mul(pow10(*sb)) {
            Some(lifted) => lifted.cmp(b),
            None => return Err(cmp_unsupported(l, r)),
        },
        // Decimal vs Double/Str goes through f64 / a string parse:
        // coarsened beyond 2^53 significands.
        // TODO(W2.0-exec): exact decimal path for Double/Str operands.
        (Decimal(a, sa), Double(b)) => decimal_to_f64(*a, *sa)
            .partial_cmp(b)
            .ok_or_else(|| cmp_unsupported(l, r))?,
        (Double(a), Decimal(b, sb)) => a
            .partial_cmp(&decimal_to_f64(*b, *sb))
            .ok_or_else(|| cmp_unsupported(l, r))?,
        (Decimal(a, sa), Str(b)) => match Value::parse_decimal(b) {
            Ok(v @ Value::Decimal(..)) => cmp_values(&Decimal(*a, *sa), &v)?,
            Ok(_) => unreachable!("parse_decimal only yields Decimal"),
            Err(_) => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("Incorrect DECIMAL value: '{b}'"),
                ))
            }
        },
        (Str(a), Decimal(b, sb)) => match Value::parse_decimal(a) {
            Ok(v @ Value::Decimal(..)) => cmp_values(&v, &Decimal(*b, *sb))?,
            Ok(_) => unreachable!("parse_decimal only yields Decimal"),
            Err(_) => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("Incorrect DECIMAL value: '{a}'"),
                ))
            }
        },
        (Bool(a), Bool(b)) => a.cmp(b),
        (Str(a), Str(b)) => a.cmp(b),
        (Bytes(a), Bytes(b)) => a.cmp(b),
        (Str(a), Bytes(b)) => a.as_bytes().cmp(b.as_slice()),
        (Bytes(a), Str(b)) => a.as_slice().cmp(b.as_bytes()),
        // Same-domain temporals compare as their underlying integers.
        (Date(a), Date(b)) => a.cmp(b),
        (DateTime(a), DateTime(b)) => a.cmp(b),
        // Cross-scale: lift days to microseconds once, compare either way.
        (Date(_), DateTime(_)) | (DateTime(_), Date(_)) => {
            let (days, us) = match (l, r) {
                (Date(d), DateTime(t)) | (DateTime(t), Date(d)) => (*d, *t),
                _ => unreachable!("guard pins the pair shape"),
            };
            days.checked_mul(MICROS_PER_DAY)
                .map(|dm| dm.cmp(&us))
                .ok_or_else(|| {
                    SqlError::new(
                        ErrorCode::NotSupported,
                        format!("cannot compare {l:?} with {r:?}"),
                    )
                })?
        }
        // A string compares against a temporal in the temporal's domain:
        // parse it canonically; anything else is an incorrect value.
        (Str(s), Date(d)) => match temporal::parse_date(s) {
            Some(p) => p.cmp(d),
            None => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("Incorrect DATE value: '{s}'"),
                ))
            }
        },
        (Date(d), Str(s)) => match temporal::parse_date(s) {
            Some(p) => d.cmp(&p),
            None => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("Incorrect DATE value: '{s}'"),
                ))
            }
        },
        (Str(s), DateTime(t)) => match temporal::parse_datetime(s) {
            Some(p) => p.cmp(t),
            None => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("Incorrect DATETIME value: '{s}'"),
                ))
            }
        },
        (DateTime(t), Str(s)) => match temporal::parse_datetime(s) {
            Some(p) => t.cmp(&p),
            None => {
                return Err(SqlError::new(
                    ErrorCode::NotSupported,
                    format!("Incorrect DATETIME value: '{s}'"),
                ))
            }
        },
        // An integer compares as the temporal's compact YYYYMMDD[HMS] form.
        (Int(_), Date(_)) | (Date(_), Int(_)) => {
            let (i, d) = match (l, r) {
                (Int(i), Date(d)) | (Date(d), Int(i)) => (*i, *d),
                _ => unreachable!("guard pins the pair shape"),
            };
            temporal::compact_date(d)
                .map(|c| {
                    if matches!(l, Int(_)) {
                        i.cmp(&c)
                    } else {
                        c.cmp(&i)
                    }
                })
                .ok_or_else(|| {
                    SqlError::new(
                        ErrorCode::NotSupported,
                        format!("cannot compare {l:?} with {r:?}"),
                    )
                })?
        }
        (Int(_), DateTime(_)) | (DateTime(_), Int(_)) => {
            let (i, t) = match (l, r) {
                (Int(i), DateTime(t)) | (DateTime(t), Int(i)) => (*i, *t),
                _ => unreachable!("guard pins the pair shape"),
            };
            temporal::compact_datetime(t)
                .map(|c| {
                    if matches!(l, Int(_)) {
                        i.cmp(&c)
                    } else {
                        c.cmp(&i)
                    }
                })
                .ok_or_else(|| {
                    SqlError::new(
                        ErrorCode::NotSupported,
                        format!("cannot compare {l:?} with {r:?}"),
                    )
                })?
        }
        (a, b) => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("cannot compare {a:?} with {b:?}"),
            ))
        }
    })
}

/// Shared "no total order here" error of `cmp_values` (used by the
/// decimal alignment fallbacks).
fn cmp_unsupported(l: &Value, r: &Value) -> SqlError {
    SqlError::new(
        ErrorCode::NotSupported,
        format!("cannot compare {l:?} with {r:?}"),
    )
}

fn eq_values(l: &Value, r: &Value) -> SqlResult<bool> {
    Ok(cmp_values(l, r)?.is_eq())
}

/// SQL LIKE with `%` (any run) and `_` (one char); `\` escapes.
/// Case sensitivity follows storage (bytewise), like MySQL's binary collation.
fn like_match(s: &str, pattern: &str) -> bool {
    fn go(s: &[char], p: &[char]) -> bool {
        match (p.first(), p.get(1)) {
            (Some('%'), Some('%')) => go(s, &p[1..]), // collapse %%
            (Some('%'), _) => {
                // try matching remainder at every suffix
                let mut i = 0;
                loop {
                    if go(&s[i..], &p[1..]) {
                        return true;
                    }
                    if i >= s.len() {
                        return false;
                    }
                    i += 1;
                }
            }
            (Some('_'), _) => !s.is_empty() && go(&s[1..], &p[1..]),
            (Some('\\'), Some(c)) => !s.is_empty() && s[0] == *c && go(&s[1..], &p[2..]),
            (Some(c), _) => !s.is_empty() && s[0] == *c && go(&s[1..], &p[1..]),
            (None, _) => s.is_empty(),
        }
    }
    go(
        &s.chars().collect::<Vec<_>>(),
        &pattern.chars().collect::<Vec<_>>(),
    )
}

fn eval_func(name: &str, args: &[Value]) -> SqlResult<Value> {
    match (name, args) {
        ("length", [v]) | ("char_length", [v]) => match v {
            // MySQL: LENGTH() counts bytes, CHAR_LENGTH() counts characters.
            Value::Str(s) => Ok(Value::Int(if name == "char_length" {
                s.chars().count() as i64
            } else {
                s.len() as i64
            })),
            Value::Bytes(b) => Ok(Value::Int(b.len() as i64)),
            Value::Null => Ok(Value::Null),
            other => Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("length({other:?})"),
            )),
        },
        ("upper", [v]) => match v {
            Value::Str(s) => Ok(Value::Str(s.to_uppercase())),
            Value::Null => Ok(Value::Null),
            other => Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("upper({other:?})"),
            )),
        },
        ("lower", [v]) => match v {
            Value::Str(s) => Ok(Value::Str(s.to_lowercase())),
            Value::Null => Ok(Value::Null),
            other => Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("lower({other:?})"),
            )),
        },
        ("abs", [v]) => match v {
            Value::Int(i) => Ok(Value::Int(i.wrapping_abs())),
            Value::Double(d) => Ok(Value::Double(d.abs())),
            // i128::MIN has no positive peer: loud, never wrapped.
            Value::Decimal(m, s) => Ok(Value::Decimal(
                m.checked_abs()
                    .ok_or_else(|| decimal_out_of_range(&format_decimal(*m, *s), *s))?,
                *s,
            )),
            Value::Null => Ok(Value::Null),
            other => Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("abs({other:?})"),
            )),
        },
        ("version", []) => Ok(Value::Str(env!("CARGO_PKG_VERSION").to_string())),
        // Clock functions: UTC wall clock (no session timezone), always
        // microsecond precision -- an fsp argument parses but is ignored.
        ("now", [] | [_])
        | ("current_timestamp", [] | [_])
        | ("sysdate", [] | [_])
        | ("localtime", [] | [_])
        | ("localtimestamp", [] | [_]) => Ok(Value::DateTime(temporal::now_micros())),
        ("curdate", [] | [_]) | ("current_date", [] | [_]) => {
            Ok(Value::Date(temporal::today_days()))
        }
        // AUTO_INCREMENT session function; see exec/sequence.rs.
        ("last_insert_id", args) => crate::sql::exec::sequence::last_insert_id_value(args),
        _ => Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("unknown function {name}"),
        )),
    }
}

/// Coerce a value for a typed column on write (INSERT/UPDATE payload).
pub fn coerce(v: Value, ty: SqlType) -> SqlResult<Value> {
    if matches!(v, Value::Null) {
        return Ok(v);
    }
    Ok(match (v, ty) {
        (Value::Bool(b), SqlType::Int) => Value::Int(i64::from(b)),
        (Value::Int(i), SqlType::Double) => Value::Double(i as f64),
        (Value::Int(i), SqlType::Bool) => Value::Bool(i != 0),
        (Value::Str(s), SqlType::Blob) => Value::Bytes(s.into_bytes()),
        // Decimal column coercion: every source first becomes an exact
        // (mantissa, scale) pair -- Ints scale up, decimals rescale,
        // strings parse, doubles go through their shortest round-trip
        // text (never the lossy f64 product) -- and the column width
        // then bounds the stored mantissa (see fit_column).
        (Value::Int(i), SqlType::Decimal { precision, scale }) => {
            fit_column(i128::from(i), 0, precision, scale, &i.to_string())?
        }
        (Value::Double(d), SqlType::Decimal { precision, scale }) => {
            if !d.is_finite() {
                return Err(decimal_out_of_range(&format!("{d}"), scale));
            }
            // Rust's f64 Display is the shortest round-tripping
            // decimal spelling; parsing it back is exact.
            let text = format!("{d}");
            match Value::parse_decimal(&text)
                .map_err(|e| SqlError::new(ErrorCode::WrongValue, e))?
            {
                Value::Decimal(m, s) => fit_column(m, s, precision, scale, &text)?,
                _ => unreachable!("parse_decimal yields Decimal"),
            }
        }
        (Value::Str(s), SqlType::Decimal { precision, scale }) => {
            match Value::parse_decimal(&s).map_err(|e| SqlError::new(ErrorCode::WrongValue, e))? {
                Value::Decimal(m, from) => fit_column(m, from, precision, scale, &s)?,
                _ => unreachable!("parse_decimal yields Decimal"),
            }
        }
        (Value::Decimal(m, s), SqlType::Decimal { precision, scale }) => {
            fit_column(m, s, precision, scale, &format_decimal(m, s))?
        }
        (Value::Decimal(m, s), SqlType::Int) => {
            let whole = m / pow10(s);
            i64::try_from(whole)
                .map(Value::Int)
                .map_err(|_| decimal_out_of_range(&format_decimal(m, s), 0))?
        }
        (Value::Decimal(m, s), SqlType::Double) => Value::Double(decimal_to_f64(m, s)),
        (Value::Decimal(m, s), SqlType::VarChar) => Value::Str(format_decimal(m, s)),
        (Value::Decimal(m, _), SqlType::Bool) => Value::Bool(m != 0),
        // Temporal coercion: strings via the canonical spellings, ints
        // via the compact YYYYMMDD[HHMMSS] forms; anything unparsable is
        // an incorrect value, not a silent NULL (MySQL 1292 style).
        (Value::Str(s), SqlType::Date) => {
            Value::Date(temporal::parse_date(&s).ok_or_else(|| incorrect_value("DATE", &s))?)
        }
        (Value::Str(s), SqlType::DateTime) => Value::DateTime(
            temporal::parse_datetime(&s).ok_or_else(|| incorrect_value("DATETIME", &s))?,
        ),
        (Value::Int(i), SqlType::Date) => Value::Date(
            temporal::parse_compact_date(i)
                .ok_or_else(|| incorrect_value("DATE", &i.to_string()))?,
        ),
        (Value::Int(i), SqlType::DateTime) => Value::DateTime(
            temporal::parse_compact_datetime(i)
                .ok_or_else(|| incorrect_value("DATETIME", &i.to_string()))?,
        ),
        // Date -> DateTime is midnight of that day; the reverse truncates.
        (Value::Date(d), SqlType::DateTime) => Value::DateTime(
            d.checked_mul(MICROS_PER_DAY)
                .ok_or_else(|| incorrect_value("DATETIME", &d.to_string()))?,
        ),
        (Value::DateTime(us), SqlType::Date) => Value::Date(us.div_euclid(MICROS_PER_DAY)),
        // Text and compact-integer renderings of a temporal.
        (Value::Date(d), SqlType::VarChar) => Value::Str(temporal::format_date(d)),
        (Value::DateTime(us), SqlType::VarChar) => Value::Str(temporal::format_datetime(us)),
        (Value::Date(d), SqlType::Int) => Value::Int(
            temporal::compact_date(d).ok_or_else(|| incorrect_value("DATE", &d.to_string()))?,
        ),
        (Value::DateTime(us), SqlType::Int) => Value::Int(
            temporal::compact_datetime(us)
                .ok_or_else(|| incorrect_value("DATETIME", &us.to_string()))?,
        ),
        (Value::Bytes(b), SqlType::VarChar) => Value::Str(String::from_utf8(b).map_err(|_| {
            SqlError::new(ErrorCode::BadNull, "blob is not valid utf8".to_string())
        })?),
        (v, t) if v.sql_type() == Some(t) => v,
        (v, t) => {
            return Err(SqlError::new(
                ErrorCode::WrongValueCount,
                format!("cannot store {v:?} into {t:?} column"),
            ))
        }
    })
}

/// Loud incorrect-value error of a temporal coercion (the parse failed).
fn incorrect_value(domain: &str, s: &str) -> SqlError {
    SqlError::new(
        ErrorCode::WrongValue,
        format!("Incorrect {domain} value: '{s}'"),
    )
}

#[cfg(test)]
#[path = "expr_tests.rs"]
mod tests;
