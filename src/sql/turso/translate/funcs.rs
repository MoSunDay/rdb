//! Scalar-function and aggregate rendering: the turso allowlist of
//! names whose SQLite semantics match the host engine's MySQL-face
//! implementations (T3 scope; `exec/func/*` is the parity reference).
//! Every other name rejects with the uniform 1235 -- including
//! near-misses that LOOK portable (LEFT/RIGHT: negative length means
//! "empty string" on MySQL but "up to end" in SQLite's substr), so
//! the ledger stays an honest allowlist.

use crate::sql::exec::render;
use crate::sql::parse::ast::{AggFunc, Expr};
use crate::sql::parse::error::SqlResult;

use super::super::engine;
use super::expr::render_expr;
use super::vector;

/// Render one function call (scalar or aggregate node). Arity was
/// already validated by the parse-time signature table, so the arms
/// below only re-state what they need for the SQLite spelling.
pub fn render_call(e: &Expr) -> SqlResult<String> {
    match e {
        Expr::Agg {
            func,
            arg,
            distinct,
            sep,
        } => {
            // GROUP_CONCAT: DISTINCT/ORDER BY/SEPARATOR semantics all
            // differ from SQLite's group_concat -- out of allowlist.
            if matches!(func, AggFunc::GroupConcat) || sep.is_some() {
                return Err(engine::turso_unsupported("aggregate GROUP_CONCAT"));
            }
            let name = match func {
                AggFunc::Count => "count",
                AggFunc::Sum => "sum",
                AggFunc::Avg => "avg",
                AggFunc::Min => "min",
                AggFunc::Max => "max",
                AggFunc::GroupConcat => unreachable!("rejected above"),
            };
            let arg_sql = match arg {
                None => "*".to_string(),
                Some(a) => format!(
                    "{}{}",
                    if *distinct { "DISTINCT " } else { "" },
                    render_expr(a)?
                ),
            };
            Ok(format!("({name}({arg_sql}))"))
        }
        Expr::Func { name, args } if name == "last_insert_id" => {
            if !args.is_empty() {
                return Err(engine::turso_unsupported(
                    "LAST_INSERT_ID(expr) (the set form)",
                ));
            }
            // Per-connection truth = the pinned turso connection's own
            // rowid watermark (see engine::session_conn); cross-database
            // USE sequences do not follow (ledger).
            Ok("last_insert_rowid()".to_string())
        }
        Expr::Func { name, args } => render_scalar(name, args),
        other => Err(engine::turso_unsupported(format!(
            "call node '{}'",
            render::expr_display(other)
        ))),
    }
}

/// One scalar call from the allowlist; arity comes pre-validated.
fn render_scalar(name: &str, args: &[Expr]) -> SqlResult<String> {
    // The vector family first: its names are not in the allowlist
    // below, they render onto turso's own vector functions.
    if let Some(sql) = vector::render_fn(name, args)? {
        return Ok(sql);
    }
    let rendered: Vec<String> = args.iter().map(render_expr).collect::<SqlResult<_>>()?;
    let sql = match (name, rendered.as_slice()) {
        // UTC on both engines (rdb renders epoch micros as civil UTC);
        // the deviation is precision: datetime('now') truncates to
        // seconds, the host engine keeps microseconds.
        ("now", _) => "datetime('now')".to_string(),
        // LENGTH is MySQL's BYTE length; SQLite's length() counts
        // characters of TEXT -- CAST(.. AS BLOB) re-bases it on bytes
        // (SQLite casts numbers to text first, so numeric operands
        // keep MySQL's text coercion too).
        ("length", [s]) => format!("(length(CAST({s} AS BLOB)))"),
        // 1-based start, negative-counts-from-the-end, len<=0 empty:
        // MySQL SUBSTRING and SQLite substr agree (func::substring).
        ("substring", [s, start]) => format!("(substr({s}, {start}))"),
        ("substring", [s, start, len]) => format!("(substr({s}, {start}, {len}))"),
        // `||` propagates NULL exactly like MySQL's CONCAT.
        ("concat", parts) => format!("({})", parts.join(" || ")),
        // mod spelled as the operator keeps SQLite's dividend-sign and
        // NULL-on-zero-divisor behavior (same as the host engine).
        ("mod", [a, b]) => format!("({a} % {b})"),
        ("abs", [x]) => format!("(abs({x}))"),
        ("upper", [x]) => format!("(upper({x}))"),
        ("lower", [x]) => format!("(lower({x}))"),
        ("round", [x]) => format!("(round({x}))"),
        ("round", [x, d]) => format!("(round({x}, {d}))"),
        ("floor", [x]) => format!("(floor({x}))"),
        ("ceil", [x]) | ("ceiling", [x]) => format!("(ceil({x}))"),
        ("coalesce", _) => format!("(coalesce({}))", rendered.join(", ")),
        ("ifnull", [a, b]) => format!("(ifnull({a}, {b}))"),
        _ => return Err(engine::turso_unsupported(format!("function '{name}'"))),
    };
    Ok(sql)
}
