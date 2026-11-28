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
        // `IS [NOT] NULL` is a postfix predicate: the parentheses wrap
        // the WHOLE test (`x IS NULL`), not the operand -- the earlier
        // `(x IS) NULL` spelling was SQLite `near ")": syntax error`
        // (MySQL face 1064); the regression tests pin the shape.
        Expr::IsNull { expr, negated } => Ok(format!(
            "({} IS{} NULL)",
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
        // Subqueries (allowlist batch 1): the inner query expression
        // renders through the compound renderer -- plain SELECT,
        // set operation or CTE body alike; SQLite resolves
        // correlated outer references natively (verified), so no
        // correlation machinery is needed on this side.
        Expr::Subquery(q) => Ok(format!("({})", super::set::render_compound(q)?)),
        Expr::Exists { query, negated } => Ok(format!(
            "({}EXISTS ({}))",
            if *negated { "NOT " } else { "" },
            super::set::render_compound(query)?
        )),
        Expr::InSubquery {
            expr,
            query,
            negated,
        } => Ok(format!(
            "({} {}IN ({}))",
            render_expr(expr)?,
            if *negated { "NOT " } else { "" },
            super::set::render_compound(query)?
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `expr IS NULL` / `expr IS NOT NULL` rendered to SQLite text.
    fn is_null(expr: Expr, negated: bool) -> SqlResult<String> {
        render_expr(&Expr::IsNull {
            expr: Box::new(expr),
            negated,
        })
    }

    fn bare(name: &str) -> Expr {
        Expr::Col {
            table: None,
            name: name.to_string(),
        }
    }

    fn qualified(table: &str, name: &str) -> Expr {
        Expr::Col {
            table: Some(table.to_string()),
            name: name.to_string(),
        }
    }

    fn add(left: Expr, right: Expr) -> Expr {
        Expr::BinaryOp {
            left: Box::new(left),
            op: BinOp::Add,
            right: Box::new(right),
        }
    }

    #[test]
    fn is_null_bare_column() {
        assert_eq!(is_null(bare("name"), false).unwrap(), "(\"name\" IS NULL)");
        assert_eq!(
            is_null(bare("name"), true).unwrap(),
            "(\"name\" IS NOT NULL)"
        );
        // Identifiers keep the shared quoting (embedded quotes doubled).
        assert_eq!(
            is_null(bare("a\"b"), false).unwrap(),
            "(\"a\"\"b\" IS NULL)"
        );
    }

    #[test]
    fn is_null_qualified_column() {
        assert_eq!(
            is_null(qualified("c", "city_id"), false).unwrap(),
            "(\"c\".\"city_id\" IS NULL)"
        );
        assert_eq!(
            is_null(qualified("customers", "city_id"), true).unwrap(),
            "(\"customers\".\"city_id\" IS NOT NULL)"
        );
    }

    #[test]
    fn is_null_nested_expression() {
        // Negation inside the predicate.
        assert_eq!(
            is_null(Expr::Neg(Box::new(bare("w"))), false).unwrap(),
            "((-\"w\") IS NULL)"
        );
        // A binary tree (fully parenthesized) inside the predicate.
        assert_eq!(
            is_null(add(Expr::Lit(Value::Int(1)), qualified("t", "x")), false).unwrap(),
            "((1 + \"t\".\"x\") IS NULL)"
        );
        assert_eq!(
            is_null(add(bare("a"), add(bare("b"), bare("c"))), true).unwrap(),
            "((\"a\" + (\"b\" + \"c\")) IS NOT NULL)"
        );
    }

    /// The defect this module pins (batch 14 R2): the old spelling put
    /// the closing parenthesis after IS / IS NOT (`(x IS) NULL`), which
    /// SQLite rejects with `near ")": syntax error`.
    #[test]
    fn is_null_never_renders_the_misplaced_paren() {
        let subjects = [
            bare("name"),
            qualified("t", "name"),
            Expr::Neg(Box::new(bare("w"))),
            add(Expr::Lit(Value::Int(1)), bare("x")),
        ];
        for negated in [false, true] {
            for e in &subjects {
                let sql = is_null(e.clone(), negated).unwrap();
                assert!(!sql.contains("IS) NULL"), "{sql}");
                assert!(!sql.contains("IS NOT) NULL"), "{sql}");
                assert!(
                    sql.ends_with(if negated { "IS NOT NULL)" } else { "IS NULL)" }),
                    "{sql}"
                );
            }
        }
    }
}
