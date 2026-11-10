//! Expression-level rendering for the turso engine: literals (every
//! rdb value kind), column refs (qualified or bare), the comparison /
//! logic / set-predicate operators SQLite spells the same way, and the
//! scalar-function allowlist in `funcs`. Every match keeps a
//! catch-all rejection (1235, the uniform COMPAT.sql.md pointer) so
//! the allowlist grows without silent semantic drift.

use crate::sql::exec::render;
use crate::sql::parse::ast::{BinOp, Expr};
use crate::sql::parse::error::SqlResult;
use crate::sql::storage::schema::{self, Value};

use super::super::engine;
use super::funcs;
use super::quote_ident;

/// Render one expression as SQLite SQL. Fully parenthesized binary
/// trees: no precedence table to maintain, and SQLite's semantics
/// match the host engine's for the supported operators (integer `/`
/// divides as integers and `/0`/`%0` yield NULL on BOTH engines --
/// the MySQL 2.5-vs-2 divide drift does not apply to this pair).
pub fn render_expr(e: &Expr) -> SqlResult<String> {
    match e {
        Expr::Lit(v) => Ok(render_value(v)),
        Expr::Col { table, name } => match table {
            // Valid only against the single FROM table (or its alias);
            // SQLite resolves and reports unknown qualifiers itself.
            Some(t) => Ok(format!("{}.{}", quote_ident(t), quote_ident(name))),
            None => Ok(quote_ident(name)),
        },
        Expr::Neg(inner) => Ok(format!("(-{})", render_expr(inner)?)),
        Expr::Not(inner) => Ok(format!("(NOT {})", render_expr(inner)?)),
        Expr::IsNull { expr, negated } => Ok(format!(
            "({} IS{}) NULL",
            render_expr(expr)?,
            if *negated { " NOT" } else { "" }
        )),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let items = list
                .iter()
                .map(render_expr)
                .collect::<SqlResult<Vec<_>>>()?
                .join(", ");
            Ok(format!(
                "({} {}IN ({}))",
                render_expr(expr)?,
                if *negated { "NOT " } else { "" },
                items
            ))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(format!(
            "({} {}BETWEEN {} AND {})",
            render_expr(expr)?,
            if *negated { "NOT " } else { "" },
            render_expr(low)?,
            render_expr(high)?
        )),
        // LIKE: both engines fold ASCII case by default; the pattern
        // rides as the plain expression it parsed to (literals most
        // often).
        Expr::Like {
            expr,
            pattern,
            negated,
        } => Ok(format!(
            "({} {}LIKE {})",
            render_expr(expr)?,
            if *negated { "NOT " } else { "" },
            render_expr(pattern)?
        )),
        Expr::BinaryOp { left, op, right } => render_binop(left, *op, right),
        Expr::Agg { .. } | Expr::Func { .. } => funcs::render_call(e),
        other => Err(engine::turso_unsupported(format!(
            "expression '{}'",
            render::expr_display(other)
        ))),
    }
}

/// One binary operator: arithmetic and comparisons map 1:1; anything
/// SQLite spells differently (or lacks) rejects loudly.
fn render_binop(left: &Expr, op: BinOp, right: &Expr) -> SqlResult<String> {
    let sql_op = match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Eq => "=",
        // MySQL's `!=` and `<>` are one operator; pick SQLite's home
        // spelling so SHOW/EXPLAIN-ish echoes stay canonical.
        BinOp::NotEq => "<>",
        BinOp::Lt => "<",
        BinOp::LtEq => "<=",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        other => {
            return Err(engine::turso_unsupported(format!(
                "operator '{}'",
                render::binop_display(other)
            )))
        }
    };
    Ok(format!(
        "({} {} {})",
        render_expr(left)?,
        sql_op,
        render_expr(right)?
    ))
}

/// Render one rdb value as a SQLite literal (shared by the translator
/// and, later, parameter replay). Follows the host renderer's display
/// spellings where MySQL and SQLite agree.
pub fn render_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        Value::Int(i) => i.to_string(),
        // Debug keeps the decimal point ("1.0" not "1"), so REAL
        // affinity survives; SQLite has no Inf/NaN literals, and it
        // treats NaN as NULL -- spell those the way the engine does.
        Value::Double(d) if d.is_infinite() => {
            if d.is_sign_negative() {
                "-9e999".to_string()
            } else {
                "9e999".to_string()
            }
        }
        Value::Double(d) if d.is_nan() => "NULL".to_string(),
        Value::Double(d) => format!("{d:?}"),
        // SQLite reads an unadorned fixed-point literal as REAL
        // (DECIMAL columns declare REAL here; the ledger carries the
        // exact-decimal deviation).
        Value::Decimal(m, s) => schema::format_decimal(*m, *s),
        Value::Date(d) => format!("'{}'", crate::sql::temporal::format_date(*d)),
        Value::DateTime(us) => {
            format!("'{}'", crate::sql::temporal::format_datetime(*us))
        }
        Value::Str(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Bytes(b) => format!("X'{}'", hex::encode(b)),
        // Vector literals ship as the raw Float32Dense blob (X'<hex
        // LE f32>'): no re-parse through vector32(), and a vector(n)
        // column's NUMERIC affinity keeps blobs blobs (text spellings
        // would stay TEXT and break length()/4 and blob read-back).
        Value::Vector(v) => super::vector::blob_literal(v),
    }
}
