//! EXPLAIN output + expression display strings.
//!
//! Pure rendering only: it walks the IR and produces text; nothing here
//! touches storage or evaluates anything.

use crate::sql::exec::{ColMeta, ExecOutcome};
use crate::sql::parse::ast::{BinOp, Expr, Query, SelectItem, Statement, TableRef};
use crate::sql::parse::error::SqlResult;
use crate::sql::storage::schema::{SqlType, Value};

/// Render one expression the way it was written, e.g. `score + 1`,
/// `t.name`, `COUNT(*)`, `x BETWEEN 1 AND 5`.
pub fn expr_display(e: &Expr) -> String {
    match e {
        Expr::Col { table, name } => match table {
            Some(t) => format!("{t}.{name}"),
            None => name.clone(),
        },
        Expr::Lit(v) => lit_display(v),
        Expr::Placeholder => "?".to_string(),
        Expr::BinaryOp { left, op, right } => {
            format!(
                "{} {} {}",
                expr_display(left),
                binop_display(*op),
                expr_display(right)
            )
        }
        Expr::Not(x) => format!("NOT {}", expr_display(x)),
        Expr::Neg(x) => format!("-{}", expr_display(x)),
        Expr::IsNull { expr, negated } => format!(
            "{} IS{} NULL",
            expr_display(expr),
            if *negated { " NOT" } else { "" }
        ),
        Expr::InList {
            expr,
            list,
            negated,
        } => {
            let items = list.iter().map(expr_display).collect::<Vec<_>>().join(", ");
            format!(
                "{} {}IN ({})",
                expr_display(expr),
                if *negated { "NOT " } else { "" },
                items
            )
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => format!(
            "{} {}BETWEEN {} AND {}",
            expr_display(expr),
            if *negated { "NOT " } else { "" },
            expr_display(low),
            expr_display(high)
        ),
        Expr::Like {
            expr,
            pattern,
            negated,
        } => format!(
            "{} {}LIKE {}",
            expr_display(expr),
            if *negated { "NOT " } else { "" },
            expr_display(pattern)
        ),
        Expr::Agg {
            func,
            arg,
            distinct,
        } => {
            let name = agg_name(func);
            match arg {
                None => format!("{name}(*)"),
                Some(a) => format!(
                    "{name}({}{})",
                    if *distinct { "DISTINCT " } else { "" },
                    expr_display(a)
                ),
            }
        }
        Expr::Func { name, args } => format!(
            "{}({})",
            name,
            args.iter().map(expr_display).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn lit_display(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Str(s) => format!("'{s}'"),
        Value::Bytes(b) => format!("x'{}'", hex::encode(b)),
    }
}

fn binop_display(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "=",
        BinOp::NotEq => "<>",
        BinOp::Lt => "<",
        BinOp::LtEq => "<=",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}

fn agg_name(f: &crate::sql::parse::ast::AggFunc) -> &'static str {
    match f {
        crate::sql::parse::ast::AggFunc::Count => "COUNT",
        crate::sql::parse::ast::AggFunc::Sum => "SUM",
        crate::sql::parse::ast::AggFunc::Avg => "AVG",
        crate::sql::parse::ast::AggFunc::Min => "MIN",
        crate::sql::parse::ast::AggFunc::Max => "MAX",
    }
}

/// One row per plan line: the whole EXPLAIN resultset.
pub fn explain(stmt: &Statement) -> SqlResult<ExecOutcome> {
    let lines = match stmt {
        Statement::Select(q) => explain_select(q),
        other => vec![format!("Direct execution ({})", other.metric_kind())],
    };
    Ok(ExecOutcome::Rows {
        columns: vec![ColMeta {
            table: String::new(),
            name: "plan".to_string(),
            sql_type: SqlType::VarChar,
        }],
        rows: lines.into_iter().map(|l| vec![Value::Str(l)]).collect(),
    })
}

fn explain_select(q: &Query) -> Vec<String> {
    let mut lines = vec![format!("SeqScan {}", from_display(&q.from))];
    let mut ons = Vec::new();
    collect_join_ons(&q.from, &mut ons);
    for on in ons {
        lines.push(format!("Join On: {}", expr_display(&on)));
    }
    if let Some(f) = &q.filter {
        lines.push(format!("Filter: {}", expr_display(f)));
    }
    if !q.group_by.is_empty() {
        let keys = q
            .group_by
            .iter()
            .map(expr_display)
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Group: {keys}"));
    }
    let mut aggs = Vec::new();
    for item in &q.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_aggs(expr, &mut aggs);
        }
    }
    if let Some(h) = &q.having {
        collect_aggs(h, &mut aggs);
    }
    // One aggregate unit per distinct function call: projection and
    // HAVING referencing COUNT(*) is a single computed unit.
    let mut seen = std::collections::BTreeSet::new();
    aggs.retain(|a| seen.insert(a.clone()));
    if !aggs.is_empty() {
        lines.push(format!("Aggregate: {}", aggs.join(", ")));
    }
    if let Some(h) = &q.having {
        lines.push(format!("Having: {}", expr_display(h)));
    }
    let items = q
        .items
        .iter()
        .map(item_display)
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("Project: {items}"));
    if !q.order_by.is_empty() {
        let keys = q
            .order_by
            .iter()
            .map(|k| {
                let dir = if k.asc { "" } else { " DESC" };
                format!("{}{}", expr_display(&k.expr), dir)
            })
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Order: {keys}"));
    }
    if q.limit.is_some() || q.offset > 0 {
        let limit = q
            .limit
            .map(|l| l.to_string())
            .unwrap_or_else(|| "all".to_string());
        lines.push(format!("Limit: {limit} Offset: {}", q.offset));
    }
    lines
}

fn from_display(t: &TableRef) -> String {
    match t {
        TableRef::Table { name, alias } => match alias {
            Some(a) => format!("{name} AS {a}"),
            None => name.clone(),
        },
        TableRef::Join { left, right, .. } => {
            format!("({} JOIN {})", from_display(left), from_display(right))
        }
    }
}

/// Collect every join condition of a FROM tree (left to right) so the
/// plan shows the nested-loop predicates.
fn collect_join_ons(t: &TableRef, out: &mut Vec<Expr>) {
    if let TableRef::Join { left, right, on } = t {
        collect_join_ons(left, out);
        if let Some(e) = on {
            out.push(e.clone());
        }
        collect_join_ons(right, out);
    }
}

fn item_display(item: &SelectItem) -> String {
    match item {
        SelectItem::Wildcard => "*".to_string(),
        SelectItem::Expr { expr, alias } => match alias {
            Some(a) => format!("{} AS {a}", expr_display(expr)),
            None => expr_display(expr),
        },
    }
}

/// Aggregates reachable in `e` (used for the EXPLAIN Aggregate line).
fn collect_aggs(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Agg { .. } => out.push(expr_display(e)),
        Expr::Lit(_) | Expr::Placeholder | Expr::Col { .. } => {}
        Expr::BinaryOp { left, right, .. } => {
            collect_aggs(left, out);
            collect_aggs(right, out);
        }
        Expr::Not(x) | Expr::Neg(x) => collect_aggs(x, out),
        Expr::IsNull { expr, .. } => collect_aggs(expr, out),
        Expr::InList { expr, list, .. } => {
            collect_aggs(expr, out);
            for item in list {
                collect_aggs(item, out);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_aggs(expr, out);
            collect_aggs(low, out);
            collect_aggs(high, out);
        }
        Expr::Like { expr, pattern, .. } => {
            collect_aggs(expr, out);
            collect_aggs(pattern, out);
        }
        Expr::Func { args, .. } => {
            for a in args {
                collect_aggs(a, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse::parse_statement;

    fn explain_lines(sql: &str) -> Vec<String> {
        // Accept both a bare statement and `EXPLAIN <stmt>`.
        let inner = match parse_statement(sql).expect("parse") {
            Statement::Explain(inner) => *inner,
            other => other,
        };
        let ExecOutcome::Rows { rows, .. } = explain(&inner).expect("explain") else {
            panic!("rows");
        };
        rows.into_iter()
            .map(|r| match r.into_iter().next() {
                Some(Value::Str(s)) => s,
                other => panic!("plan line is {other:?}"),
            })
            .collect()
    }

    #[test]
    fn explain_select_lines() {
        let lines = explain_lines(
            "SELECT id, COUNT(*) AS n FROM users WHERE score > 1.5 GROUP BY id \
             HAVING COUNT(*) > 1 ORDER BY n DESC LIMIT 5 OFFSET 2",
        );
        assert_eq!(lines[0], "SeqScan users");
        assert_eq!(lines[1], "Filter: score > 1.5");
        assert_eq!(lines[2], "Group: id");
        assert_eq!(lines[3], "Aggregate: COUNT(*)");
        assert_eq!(lines[4], "Having: COUNT(*) > 1");
        assert_eq!(lines[5], "Project: id, COUNT(*) AS n");
        assert_eq!(lines[6], "Order: n DESC");
        assert_eq!(lines[7], "Limit: 5 Offset: 2");
    }

    #[test]
    fn explain_join_and_direct() {
        let lines = explain_lines("SELECT u.id FROM u JOIN o ON u.id = o.uid");
        assert_eq!(lines[0], "SeqScan (u JOIN o)");
        assert_eq!(lines[1], "Join On: u.id = o.uid");

        // Non-SELECT statements plan as one direct-execution row.
        let stmt = parse_statement("INSERT INTO t (a) VALUES (1)").unwrap();
        let ExecOutcome::Rows { rows, .. } = explain(&stmt).expect("explain") else {
            panic!("rows");
        };
        assert_eq!(
            rows[0][0],
            Value::Str("Direct execution (insert)".to_string())
        );
    }

    #[test]
    fn expr_display_rendering() {
        let sql = "SELECT a + 1, b NOT LIKE 'x%', c IS NOT NULL FROM t";
        let Statement::Select(q) = parse_statement(sql).unwrap() else {
            panic!("shape");
        };
        let rendered: Vec<String> = q
            .items
            .iter()
            .map(|i| match i {
                SelectItem::Expr { expr, .. } => expr_display(expr),
                SelectItem::Wildcard => "*".to_string(),
            })
            .collect();
        assert_eq!(rendered, vec!["a + 1", "b NOT LIKE 'x%'", "c IS NOT NULL"]);
    }
}
