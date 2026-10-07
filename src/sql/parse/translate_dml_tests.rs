//! IR-shape coverage for the INSERT family: ODKU (with the VALUES()
//! marker), REPLACE, INSERT ... SET normalization, INSERT ... SELECT
//! sources, and the parse-time rejections (VALUES() outside ODKU,
//! REPLACE + ODKU, unknown clause forms).

use crate::sql::parse::ast::{ConflictAction, Expr, InsertSource, Statement};
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::translate::{bind_placeholders, parse_statement, placeholder_count};
use crate::sql::storage::schema::Value;

fn stmt(sql: &str) -> Statement {
    parse_statement(sql).expect("parse")
}

fn insert_of(sql: &str) -> (Vec<String>, InsertSource, ConflictAction) {
    let Statement::Insert {
        columns,
        source,
        conflict,
        ..
    } = stmt(sql)
    else {
        panic!("insert shape");
    };
    (columns, source, conflict)
}

#[test]
fn plain_values_shape_is_unchanged() {
    let (cols, source, conflict) = insert_of("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')");
    assert_eq!(cols, vec!["a", "b"]);
    let InsertSource::Values(rows) = source else {
        panic!("values source");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Expr::Lit(Value::Int(1)));
    assert!(matches!(conflict, ConflictAction::Error));
}

#[test]
fn odku_translates_assignments_with_values_marker() {
    let (_, source, conflict) = insert_of(
        "INSERT INTO t (a, b) VALUES (1, 2) ON DUPLICATE KEY UPDATE b = VALUES(b) + 1, a = 9",
    );
    assert!(matches!(source, InsertSource::Values(_)));
    let ConflictAction::OnDuplicate(assigns) = conflict else {
        panic!("odku");
    };
    assert_eq!(assigns.len(), 2);
    assert_eq!(assigns[0].0, "b");
    // VALUES(b) + 1 keeps the binary shape with the marker inside
    let Expr::BinaryOp { left, .. } = &assigns[0].1 else {
        panic!("binary");
    };
    assert_eq!(*left.clone(), Expr::InsertValues("b".into()));
    // plain constants pass through untouched
    assert_eq!(assigns[1].1, Expr::Lit(Value::Int(9)));
}

#[test]
fn replace_marks_the_conflict_action() {
    let (_, _, conflict) = insert_of("REPLACE INTO t (a) VALUES (1)");
    assert!(matches!(conflict, ConflictAction::Replace));
}

#[test]
fn insert_set_normalizes_to_named_single_row() {
    let (cols, source, conflict) = insert_of("INSERT INTO t SET a = 1, b = 'x'");
    assert_eq!(cols, vec!["a", "b"]);
    let InsertSource::Values(rows) = source else {
        panic!("values source");
    };
    assert_eq!(
        rows,
        vec![vec![
            Expr::Lit(Value::Int(1)),
            Expr::Lit(Value::Str("x".into()))
        ]]
    );
    assert!(matches!(conflict, ConflictAction::Error));
    // SET + ODKU composes (MySQL allows the pair)
    let (_, _, conflict) =
        insert_of("INSERT INTO t SET a = 1 ON DUPLICATE KEY UPDATE a = VALUES(a)");
    assert!(matches!(conflict, ConflictAction::OnDuplicate(_)));
}

#[test]
fn select_source_translates_to_compound_query() {
    let (cols, source, conflict) =
        insert_of("INSERT INTO t (a, b) SELECT x, y + 1 FROM o WHERE x > 3");
    assert_eq!(cols, vec!["a", "b"]);
    let InsertSource::Select(cq) = source else {
        panic!("select source");
    };
    let crate::sql::parse::ast::QueryBody::Select(q) = &cq.body else {
        panic!("plain select body");
    };
    assert_eq!(q.items.len(), 2);
    assert!(q.filter.is_some());
    assert!(matches!(conflict, ConflictAction::Error));
}

#[test]
fn values_fn_outside_odku_is_a_parse_error() {
    for sql in [
        "INSERT INTO t (a) VALUES (VALUES(a))",
        "INSERT INTO t SET a = VALUES(a)",
    ] {
        let err = parse_statement(sql).expect_err("rejected");
        assert_eq!(err.code, ErrorCode::Parse, "{sql}: {err}");
        assert!(
            err.msg.contains("only allowed in ON DUPLICATE KEY UPDATE"),
            "{sql}: {err}"
        );
    }
}

#[test]
fn replace_plus_odku_rejects() {
    let err = parse_statement("REPLACE INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = 1")
        .expect_err("rejected");
    assert!(err.msg.contains("REPLACE"), "{err}");
}

#[test]
fn placeholders_count_and_bind_across_sources_and_odku() {
    let mut s =
        stmt("INSERT INTO t (a, b) VALUES (?, ?) ON DUPLICATE KEY UPDATE b = VALUES(b) + ?");
    assert_eq!(placeholder_count(&s), 3);
    bind_placeholders(
        &mut s,
        &[Value::Int(1), Value::Str("x".into()), Value::Int(5)],
    )
    .expect("bind");
    let (_, source, conflict) = match &s {
        Statement::Insert {
            columns,
            source,
            conflict,
            ..
        } => (columns.clone(), source.clone(), conflict.clone()),
        _ => panic!("insert"),
    };
    let InsertSource::Values(rows) = source else {
        panic!("values");
    };
    assert_eq!(rows[0][0], Expr::Lit(Value::Int(1)));
    let ConflictAction::OnDuplicate(assigns) = conflict else {
        panic!("odku");
    };
    let Expr::BinaryOp { right, .. } = &assigns[0].1 else {
        panic!("binary");
    };
    assert_eq!(**right, Expr::Lit(Value::Int(5)));
    // placeholders inside a SELECT source count AND bind (in text order)
    let mut s2 = stmt("INSERT INTO t (a) SELECT x FROM o WHERE x > ?");
    assert_eq!(placeholder_count(&s2), 1);
    bind_placeholders(&mut s2, &[Value::Int(7)]).expect("bind select source");
    let source = match &s2 {
        Statement::Insert { source, .. } => source.clone(),
        _ => panic!("insert"),
    };
    let InsertSource::Select(cq) = source else {
        panic!("select source");
    };
    let crate::sql::parse::ast::QueryBody::Select(q) = cq.body else {
        panic!("body");
    };
    assert_eq!(
        q.filter,
        Some(Expr::BinaryOp {
            left: Box::new(Expr::Col {
                table: None,
                name: "x".into()
            }),
            op: crate::sql::parse::ast::BinOp::Gt,
            right: Box::new(Expr::Lit(Value::Int(7))),
        })
    );
}

#[test]
fn row_arity_still_rejects_at_parse() {
    let err = parse_statement("INSERT INTO t (a, b) VALUES (1)").expect_err("arity");
    assert_eq!(err.code, ErrorCode::WrongValueCount, "{err}");
    let err = parse_statement("INSERT INTO t (a) VALUES ()").expect_err("empty tuple");
    assert_eq!(err.code, ErrorCode::WrongValueCount, "{err}");
}

#[test]
fn sqlite_upsert_and_row_alias_reject() {
    let err = parse_statement("INSERT OR REPLACE INTO t (a) VALUES (1)").expect_err("or");
    assert_eq!(err.code, ErrorCode::NotSupported, "{err}");
}
