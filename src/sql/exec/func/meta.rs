//! Static result-column type of a scalar function call: a pure
//! `name -> SqlType` table (plus argument types for the
//! input-mirroring entries) consulted by the projection typer.
//! Metadata only -- evaluation stays fully dynamic.
//!
//! Kept in lockstep with the family evaluators and the translate-time
//! arity table (`parse::func_sig`): every name the evaluator owns has
//! an entry here, even when it just types as text.

use crate::sql::parse::ast::Expr;
use crate::sql::storage::schema::{SqlType, Value, MAX_DECIMAL_SCALE};

/// Widest-numeric helper shared by the arithmetic-mirroring entries:
/// Double > Decimal(coarser scale) > Int, like the BinOp typer.
fn widest(types: &[SqlType]) -> SqlType {
    if types.contains(&SqlType::Double) {
        return SqlType::Double;
    }
    let scale = types
        .iter()
        .filter_map(|t| match t {
            SqlType::Decimal { scale, .. } => Some(*scale),
            _ => None,
        })
        .max();
    match scale {
        Some(s) => SqlType::Decimal {
            precision: MAX_DECIMAL_SCALE,
            scale: s,
        },
        None => SqlType::Int,
    }
}

/// Literal Int of an argument expression (ROUND/TRUNCATE's `d` slot),
/// when statically known.
fn lit_int(e: &Expr) -> Option<i64> {
    match e {
        Expr::Lit(Value::Int(i)) => Some(*i),
        Expr::Neg(inner) => lit_int(inner).map(|i| i.wrapping_neg()),
        _ => None,
    }
}

/// Decimal scale cap for ROUND: `min(d, arg-scale)` when d is a known
/// literal, else the argument scale.
fn round_scale(d_lit: Option<i64>, arg: SqlType) -> u8 {
    let arg_scale = match arg {
        SqlType::Decimal { scale, .. } => scale,
        _ => return 0,
    };
    match d_lit {
        Some(d) if d >= 0 => u8::try_from(d).unwrap_or(arg_scale).min(arg_scale),
        Some(_) => 0, // negative d scales the integer part down
        None => arg_scale,
    }
}

/// Result type of `name(args)`; `ty` resolves one argument's type
/// (recursively, scope-aware -- supplied by the caller).
pub fn result_type(name: &str, args: &[Expr], ty: &dyn Fn(&Expr) -> SqlType) -> SqlType {
    let lower = name.to_lowercase();
    let name = lower.as_str();
    let first = || args.first().map(ty);
    match name {
        // Int answers.
        "length" | "char_length" | "locate" | "instr" | "sign" | "datediff" | "unix_timestamp"
        | "year" | "month" | "day" | "dayofmonth" | "hour" | "minute" | "second"
        | "last_insert_id" => SqlType::Int,
        // Clock family.
        "now" | "current_timestamp" | "sysdate" | "localtime" | "localtimestamp" => {
            SqlType::DateTime
        }
        "curdate" | "current_date" => SqlType::Date,
        "curtime" | "current_time" | "from_unixtime" | "date_format" => SqlType::VarChar,
        "date" => SqlType::Date,
        // Date arithmetic mirrors its input shape: Date stays Date
        // (time units promote, unknowable statically), DateTime stays
        // DateTime, text/int inputs answer in their own spelling.
        "date_add" | "date_sub" | "adddate" | "subdate" => match first() {
            Some(SqlType::Date) => SqlType::Date,
            Some(SqlType::DateTime) => SqlType::DateTime,
            Some(SqlType::Int) => SqlType::Int,
            _ => SqlType::VarChar,
        },
        // Rounding set: type mirrors the input.
        "ceil" | "ceiling" | "floor" => match first() {
            Some(SqlType::Decimal { .. }) => SqlType::Decimal {
                precision: MAX_DECIMAL_SCALE,
                scale: 0,
            },
            other => other.unwrap_or(SqlType::Int),
        },
        "round" => {
            let arg = first().unwrap_or(SqlType::Int);
            match arg {
                SqlType::Decimal { .. } => SqlType::Decimal {
                    precision: MAX_DECIMAL_SCALE,
                    scale: round_scale(args.get(1).and_then(lit_int), arg),
                },
                other => other,
            }
        }
        "truncate" => {
            let arg = first().unwrap_or(SqlType::Int);
            match arg {
                SqlType::Decimal { .. } => SqlType::Decimal {
                    precision: MAX_DECIMAL_SCALE,
                    scale: round_scale(args.get(1).and_then(lit_int), arg),
                },
                other => other,
            }
        }
        "mod" | "greatest" | "least" => widest(&args.iter().map(ty).collect::<Vec<_>>()),
        // Floating answers.
        "pow" | "power" | "sqrt" => SqlType::Double,
        // Lazy control family: first non-NULL-ish argument type wins.
        "if" | "ifnull" | "nullif" | "coalesce" => {
            let types: Vec<SqlType> = args.iter().map(ty).collect();
            widest_textual(&types)
        }
        // Binary-string answer (hex decode).
        "unhex" => SqlType::Blob,
        // Everything else in the string family answers text.
        _ => SqlType::VarChar,
    }
}

/// Widest across the mixed text/numeric domain (the lazy control
/// functions accept anything): any text branch wins (VarChar), else
/// Double > Decimal > Int.
fn widest_textual(types: &[SqlType]) -> SqlType {
    // Any non-numeric branch (text, temporal, bool) makes the column
    // text: a Str value on an Int column would mislabel the wire cell.
    let any_text = types
        .iter()
        .any(|t| !matches!(t, SqlType::Double | SqlType::Decimal { .. } | SqlType::Int));
    if any_text {
        return SqlType::VarChar;
    }
    widest(types)
}

#[cfg(test)]
#[path = "meta_tests.rs"]
mod tests;
