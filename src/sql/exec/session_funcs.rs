//! Session scalar functions: `DATABASE()` / `SCHEMA()`, `USER()` /
//! `CURRENT_USER()` / `SESSION_USER()` and `CONNECTION_ID()`.
//!
//! These are the only expressions that depend on CONNECTION state, and
//! the expression evaluator is deliberately session-free (its scope is
//! a pure column-resolution interface). Instead of threading a session
//! parameter through every `eval` call site, the binding happens ONCE
//! PER EXECUTION at the executor entry ([`super::execute`]): each
//! zero-argument session function call is replaced in place by its
//! literal value. That keeps evaluation pure while preserving MySQL's
//! semantics exactly:
//!
//! - prepared statements re-run after `USE` see the CURRENT database
//!   (substitution runs on every EXECUTE, not at PREPARE);
//! - the value is one snapshot per statement (consistent across rows),
//!   which is also MySQL's behavior -- `USER()` does not change
//!   mid-statement.
//!
//! Arity is rejected at translate time (`parse::func_sig` owns the
//! (0,0) entries), so the substitution only ever sees empty argument
//! lists; a stray call with arguments that reaches evaluation keeps the
//! generic "unknown function" error.

use super::SessionInfo;
use crate::sql::parse::ast::{
    CompoundQuery, ConflictAction, Expr, InsertSource, Query, QueryBody, Statement, TableRef,
};
use crate::sql::storage::schema::Value;

/// What the session functions read: the current database (already
/// defaulted -- an empty `USE` target means the engine's implicit db)
/// plus the immutable connection identity. Built per execution from
/// the [`SqlSession`]; plain data.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionState {
    pub db: String,
    pub info: SessionInfo,
}

/// Rewrite every session function call in `stmt` to its literal value.
///
/// Every surface that can host an expression is covered: SELECT
/// items / WHERE / GROUP BY / HAVING / ORDER BY (+ the FROM tree and
/// subqueries), INSERT row cells and `INSERT .. SELECT` sources plus
/// the ON DUPLICATE KEY UPDATE assignments, and UPDATE / DELETE
/// assignments, filters and ORDER BY keys.
pub(crate) fn substitute(stmt: &mut Statement, sess: &SessionState) {
    match stmt {
        Statement::Select(q) => substitute_query(q, sess),
        Statement::SelectCompound(cq) => substitute_compound(cq, sess),
        Statement::Explain(inner) => substitute(inner, sess),
        Statement::Insert {
            source, conflict, ..
        } => {
            match source {
                InsertSource::Values(rows) => {
                    for row in rows {
                        for cell in row.iter_mut() {
                            substitute_expr(cell, sess);
                        }
                    }
                }
                InsertSource::Select(cq) => substitute_compound(cq, sess),
            }
            // ODKU assignments evaluate in the same statement: a
            // `v = DATABASE()` there needs the literal too, or the
            // session-free evaluator rejects the call.
            if let ConflictAction::OnDuplicate(assignments) = conflict {
                for (_, e) in assignments.iter_mut() {
                    substitute_expr(e, sess);
                }
            }
        }
        Statement::Update {
            assignments,
            filter,
            order_by,
            ..
        } => {
            for (_, e) in assignments.iter_mut() {
                substitute_expr(e, sess);
            }
            if let Some(f) = filter.as_mut() {
                substitute_expr(f, sess);
            }
            for k in order_by.iter_mut() {
                substitute_expr(&mut k.expr, sess);
            }
        }
        Statement::Delete {
            filter, order_by, ..
        } => {
            if let Some(f) = filter.as_mut() {
                substitute_expr(f, sess);
            }
            for k in order_by.iter_mut() {
                substitute_expr(&mut k.expr, sess);
            }
        }
        _ => {}
    }
}

fn substitute_query(q: &mut Query, sess: &SessionState) {
    for item in q.items.iter_mut() {
        if let crate::sql::parse::ast::SelectItem::Expr { expr, .. } = item {
            substitute_expr(expr, sess);
        }
    }
    if let Some(f) = q.filter.as_mut() {
        substitute_expr(f, sess);
    }
    for g in q.group_by.iter_mut() {
        substitute_expr(g, sess);
    }
    if let Some(h) = q.having.as_mut() {
        substitute_expr(h, sess);
    }
    for k in q.order_by.iter_mut() {
        substitute_expr(&mut k.expr, sess);
    }
    substitute_from(&mut q.from, sess);
}

fn substitute_from(t: &mut TableRef, sess: &SessionState) {
    match t {
        TableRef::Derived { query, .. } => substitute_compound(query, sess),
        TableRef::Join { left, right, .. } => {
            substitute_from(left, sess);
            substitute_from(right, sess);
        }
        TableRef::Table { .. } | TableRef::NoTable => {}
    }
}

fn substitute_compound(cq: &mut CompoundQuery, sess: &SessionState) {
    for cte in cq.ctes.iter_mut() {
        substitute_compound(&mut cte.query, sess);
    }
    substitute_body(&mut cq.body, sess);
    for k in cq.order_by.iter_mut() {
        substitute_expr(&mut k.expr, sess);
    }
}

fn substitute_body(b: &mut QueryBody, sess: &SessionState) {
    match b {
        QueryBody::Select(q) => substitute_query(q, sess),
        QueryBody::Nested(inner) => substitute_compound(inner, sess),
        QueryBody::SetOp { left, right, .. } => {
            substitute_body(left, sess);
            substitute_body(right, sess);
        }
    }
}

/// One session function's literal for this execution, or `None` when
/// the name is not a session function. `args` must be empty (the
/// translate-time arity table guarantees it).
fn literal(name: &str, sess: &SessionState) -> Option<Value> {
    match name {
        "database" | "schema" => Some(Value::Str(sess.db.clone())),
        "user" | "current_user" | "session_user" => Some(Value::Str(sess.info.user.clone())),
        "connection_id" => Some(Value::Int(sess.info.connection_id)),
        _ => None,
    }
}

/// Depth-first rewrite of one expression tree: every session function
/// call becomes its literal (in place; nothing else changes).
fn substitute_expr(e: &mut Expr, sess: &SessionState) {
    match e {
        Expr::Func { name, args } => {
            if args.is_empty() {
                if let Some(v) = literal(name, sess) {
                    *e = Expr::Lit(v);
                    return;
                }
            }
            for a in args.iter_mut() {
                substitute_expr(a, sess);
            }
        }
        Expr::Col { .. } | Expr::Lit(_) | Expr::Placeholder | Expr::InsertValues(_) => {}
        Expr::BinaryOp { left, right, .. } => {
            substitute_expr(left, sess);
            substitute_expr(right, sess);
        }
        Expr::Not(x) | Expr::Neg(x) => substitute_expr(x, sess),
        Expr::IsNull { expr, .. } => substitute_expr(expr, sess),
        Expr::InList { expr, list, .. } => {
            substitute_expr(expr, sess);
            for item in list.iter_mut() {
                substitute_expr(item, sess);
            }
        }
        Expr::Subquery(cq) => substitute_compound(cq, sess),
        Expr::InSubquery { expr, query, .. } => {
            substitute_expr(expr, sess);
            substitute_compound(query, sess);
        }
        Expr::Exists { query, .. } => substitute_compound(query, sess),
        Expr::Correlated { keys, .. } => {
            for k in keys.iter_mut() {
                substitute_expr(k, sess);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            substitute_expr(expr, sess);
            substitute_expr(low, sess);
            substitute_expr(high, sess);
        }
        Expr::Like { expr, pattern, .. } => {
            substitute_expr(expr, sess);
            substitute_expr(pattern, sess);
        }
        Expr::Regexp { expr, pattern, .. } => {
            substitute_expr(expr, sess);
            substitute_expr(pattern, sess);
        }
        Expr::Case {
            operand,
            branches,
            else_expr,
        } => {
            if let Some(op) = operand.as_deref_mut() {
                substitute_expr(op, sess);
            }
            for (when, then) in branches.iter_mut() {
                substitute_expr(when, sess);
                substitute_expr(then, sess);
            }
            if let Some(el) = else_expr.as_deref_mut() {
                substitute_expr(el, sess);
            }
        }
        Expr::Cast { expr, .. } => substitute_expr(expr, sess),
        Expr::Agg { arg, .. } => {
            if let Some(a) = arg.as_deref_mut() {
                substitute_expr(a, sess);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse::parse_statement;

    fn sess(db: &str, user: &str, id: i64) -> SessionState {
        SessionState {
            db: db.to_string(),
            info: SessionInfo {
                user: user.to_string(),
                connection_id: id,
            },
        }
    }

    fn cell(stmt: &Statement) -> Value {
        let Statement::Select(q) = stmt else {
            panic!("select");
        };
        match &q.items[0] {
            crate::sql::parse::ast::SelectItem::Expr { expr, .. } => match expr {
                Expr::Lit(v) => v.clone(),
                other => panic!("not substituted: {other:?}"),
            },
            _ => panic!("expr item"),
        }
    }

    #[test]
    fn substitutes_the_three_families() {
        let s = sess("mydb", "root@127.0.0.1", 7);
        for (sql, want) in [
            ("SELECT DATABASE()", Value::Str("mydb".into())),
            ("SELECT SCHEMA()", Value::Str("mydb".into())),
            ("SELECT USER()", Value::Str("root@127.0.0.1".into())),
            ("SELECT CURRENT_USER()", Value::Str("root@127.0.0.1".into())),
            ("SELECT SESSION_USER()", Value::Str("root@127.0.0.1".into())),
            ("SELECT CONNECTION_ID()", Value::Int(7)),
        ] {
            let mut stmt = parse_statement(sql).unwrap();
            substitute(&mut stmt, &s);
            assert_eq!(cell(&stmt), want, "{sql}");
        }
    }

    #[test]
    fn reexecution_sees_the_new_database() {
        // The prepare-time parse keeps the call; each substitute run
        // binds the CURRENT session state (the USE between runs).
        let mut stmt = parse_statement("SELECT DATABASE()").unwrap();
        substitute(&mut stmt, &sess("a", "u@h", 1));
        assert_eq!(cell(&stmt), Value::Str("a".into()));
        let mut stmt = parse_statement("SELECT DATABASE()").unwrap();
        substitute(&mut stmt, &sess("b", "u@h", 1));
        assert_eq!(cell(&stmt), Value::Str("b".into()));
    }

    #[tokio::test]
    async fn end_to_end_through_execute_with_fake_session() {
        use crate::sql::exec::{self, ExecOutcome, SqlSession};
        use crate::state::testutil;
        let shared = testutil::shared_with(testutil::test_config());
        let mut sess = SqlSession {
            db: "probedb".into(),
            info: SessionInfo {
                user: "probe@10.0.0.9".into(),
                connection_id: 42,
            },
            ..Default::default()
        };
        let out = exec::execute(
            &shared,
            &mut sess,
            parse_statement("SELECT DATABASE(), USER(), CONNECTION_ID()").unwrap(),
        )
        .await
        .unwrap();
        let ExecOutcome::Rows { rows, .. } = out else {
            panic!("rows");
        };
        assert_eq!(
            rows[0],
            vec![
                Value::Str("probedb".into()),
                Value::Str("probe@10.0.0.9".into()),
                Value::Int(42)
            ]
        );
        // no USE ran on this session: the engine default answers
        let mut bare = SqlSession::default();
        let out = exec::execute(
            &shared,
            &mut bare,
            parse_statement("SELECT DATABASE()").unwrap(),
        )
        .await
        .unwrap();
        let ExecOutcome::Rows { rows, .. } = out else {
            panic!("rows");
        };
        assert_eq!(
            rows[0],
            vec![Value::Str(crate::sql::exec::DEFAULT_DB.into())]
        );
        // EXPLAIN sees the substituted literals too (typing, not eval)
        let out = exec::execute(
            &shared,
            &mut sess,
            parse_statement("EXPLAIN SELECT DATABASE()").unwrap(),
        )
        .await
        .unwrap();
        assert!(matches!(out, ExecOutcome::Rows { .. }));
    }

    #[test]
    fn substitutes_inside_filters_and_subqueries() {
        let s = sess("d", "u@h", 3);
        let mut stmt =
            parse_statement("SELECT v FROM t WHERE id = CONNECTION_ID() AND v = DATABASE()")
                .unwrap();
        substitute(&mut stmt, &s);
        // both calls became literals: the filter is a pure literal tree
        let Statement::Select(q) = &stmt else {
            panic!("select");
        };
        let text = format!("{:?}", q.filter);
        assert!(text.contains("Int(3)"), "{text}");
        assert!(text.contains("Str(\"d\")"), "{text}");

        let mut stmt =
            parse_statement("SELECT (SELECT max(x) FROM t2 WHERE u = USER()) FROM t3").unwrap();
        substitute(&mut stmt, &s);
    }

    #[test]
    fn substitutes_odku_assignments() {
        let s = sess("d", "u@h", 3);
        let mut stmt = parse_statement(
            "INSERT INTO t (id, v, k) VALUES (1, 'a', 'b') \
             ON DUPLICATE KEY UPDATE v = DATABASE(), k = VALUES(k)",
        )
        .unwrap();
        substitute(&mut stmt, &s);
        let Statement::Insert {
            conflict: ConflictAction::OnDuplicate(assigns),
            ..
        } = &stmt
        else {
            panic!("odku insert, got {stmt:?}");
        };
        let by_col = |c: &str| {
            assigns
                .iter()
                .find(|(col, _)| col == c)
                .unwrap_or_else(|| panic!("no assignment for {c}"))
                .1
                .clone()
        };
        // the session call became the literal db name ...
        assert_eq!(by_col("v"), Expr::Lit(Value::Str("d".into())));
        // ... while the VALUES(col) marker (bound per incoming row by
        // the upsert path, not by the session) survives untouched.
        assert_eq!(by_col("k"), Expr::InsertValues("k".into()));
    }

    #[test]
    fn substitutes_update_and_delete_order_by() {
        let s = sess("d", "u@h", 3);
        let mut stmt =
            parse_statement("UPDATE t SET v = USER() ORDER BY DATABASE() LIMIT 2").unwrap();
        substitute(&mut stmt, &s);
        let Statement::Update {
            assignments,
            order_by,
            ..
        } = &stmt
        else {
            panic!("update, got {stmt:?}");
        };
        assert_eq!(assignments[0].1, Expr::Lit(Value::Str("u@h".into())));
        assert_eq!(order_by[0].expr, Expr::Lit(Value::Str("d".into())));

        let mut stmt = parse_statement("DELETE FROM t ORDER BY CONNECTION_ID() LIMIT 1").unwrap();
        substitute(&mut stmt, &s);
        let Statement::Delete { order_by, .. } = &stmt else {
            panic!("delete, got {stmt:?}");
        };
        assert_eq!(order_by[0].expr, Expr::Lit(Value::Int(3)));
    }
}
