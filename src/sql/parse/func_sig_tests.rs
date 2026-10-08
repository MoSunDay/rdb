//! Translate-time tests: the signature table (arity) and the parse
//! shapes of the M1 groundwork nodes (CASE/CAST/CONVERT, REGEXP,
//! `<=>`/XOR/bit operators).

use super::*;
use crate::sql::parse::ast::{BinOp, CastSpec, Expr, SelectItem, Statement};
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::parse_statement;
use crate::sql::storage::schema::Value;

fn first_expr(sql: &str) -> Expr {
    let Statement::Select(q) = parse_statement(sql).expect("parse") else {
        panic!("select");
    };
    let SelectItem::Expr { expr, .. } = q.items.into_iter().next().expect("item") else {
        panic!("expr item");
    };
    expr
}

fn binop(sql: &str) -> (Box<Expr>, BinOp, Box<Expr>) {
    let Expr::BinaryOp { left, op, right } = first_expr(sql) else {
        panic!("binary op");
    };
    (left, op, right)
}

// Probe-verified mapping of the operator tokens the MySQL dialect
// emits (sqlparser tags `<<`/`>>` with the PG-prefixed variants).
#[test]
fn operators_translate_to_ir_binops() {
    use BinOp::*;
    for (sql, want) in [
        ("SELECT a <=> b", NullSafeEq),
        ("SELECT a XOR b", LogicalXor),
        ("SELECT a & b", BitAnd),
        ("SELECT a | b", BitOr),
        ("SELECT a ^ b", BitXor),
        ("SELECT a << 2", Shl),
        ("SELECT a >> 2", Shr),
    ] {
        let (_, op, _) = binop(sql);
        assert_eq!(op, want, "{sql}");
    }
}

#[test]
fn case_both_forms_translate() {
    let Expr::Case {
        operand,
        branches,
        else_expr,
    } = first_expr("SELECT CASE WHEN a = 1 THEN 'x' ELSE 'y' END")
    else {
        panic!("case");
    };
    assert!(operand.is_none());
    assert_eq!(branches.len(), 1);
    assert!(matches!(&branches[0].1, Expr::Lit(Value::Str(_))));
    assert!(else_expr.is_some());

    let Expr::Case {
        operand,
        branches,
        else_expr,
    } = first_expr("SELECT CASE a WHEN 1 THEN 'x' WHEN 2 THEN 'y' END")
    else {
        panic!("case");
    };
    assert!(operand.is_some());
    assert_eq!(branches.len(), 2);
    assert!(else_expr.is_none());
}

#[test]
fn cast_and_convert_translate() {
    for (sql, want) in [
        ("SELECT CAST(x AS SIGNED)", CastSpec::Signed),
        ("SELECT CAST(x AS UNSIGNED)", CastSpec::Unsigned),
        ("SELECT CAST(x AS CHAR)", CastSpec::Char(None)),
        ("SELECT CAST(x AS CHAR(5))", CastSpec::Char(Some(5))),
        (
            "SELECT CAST(x AS DECIMAL(10,2))",
            CastSpec::Decimal {
                precision: 10,
                scale: 2,
            },
        ),
        ("SELECT CONVERT(x, SIGNED)", CastSpec::Signed),
        ("SELECT CONVERT(x USING utf8mb4)", CastSpec::Char(None)),
    ] {
        let Expr::Cast { to, .. } = first_expr(sql) else {
            panic!("cast: {sql}");
        };
        assert_eq!(to, want, "{sql}");
    }
    // Wider targets reject loudly (DOUBLE/BINARY/DATE cast targets,
    // non-identity charsets, TRY_CAST forms).
    for sql in [
        "SELECT CAST(x AS DOUBLE)",
        "SELECT CAST(x AS BINARY(4))",
        "SELECT CAST(x AS DATE)",
        "SELECT CONVERT(x USING latin1)",
        "SELECT TRY_CAST(x AS SIGNED)",
        "SELECT CAST(x AS DECIMAL(50,2))",
        "SELECT CAST(x AS DECIMAL(2,5))",
    ] {
        let e = parse_statement(sql).expect_err("unsupported");
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
    }
}

#[test]
fn regexp_translates_with_negation() {
    let Expr::Regexp { negated, .. } = first_expr("SELECT a REGEXP '^x'") else {
        panic!("regexp");
    };
    assert!(!negated);
    let Expr::Regexp { negated, .. } = first_expr("SELECT a RLIKE '^x'") else {
        panic!("rlike");
    };
    assert!(!negated);
    let Expr::Regexp { negated, .. } = first_expr("SELECT a NOT REGEXP '^x'") else {
        panic!("not regexp");
    };
    assert!(negated);
    // `NOT a REGEXP b` parses as Not(Regexp) and translates as such.
    assert!(matches!(
        first_expr("SELECT NOT a REGEXP '^x'"),
        Expr::Not(_)
    ));
}

#[test]
fn placeholders_inside_new_nodes_count_and_bind() {
    // Two `?` inside CASE + one in the CAST -> three parameters.
    let stmt =
        parse_statement("SELECT CASE WHEN a = ? THEN ? ELSE 0 END, CAST(? AS SIGNED) FROM t")
            .expect("parse");
    assert_eq!(crate::sql::parse::translate::placeholder_count(&stmt), 3);
    let mut bound = stmt;
    crate::sql::parse::bind_placeholders(
        &mut bound,
        &[Value::Int(1), Value::Int(5), Value::Str("9".into())],
    )
    .expect("bind");
    let Statement::Select(q) = &bound else {
        panic!("select");
    };
    let SelectItem::Expr {
        expr: Expr::Case { branches, .. },
        ..
    } = &q.items[0]
    else {
        panic!("case item");
    };
    // The first `?` sits inside the WHEN condition (a = 1), the
    // second is the THEN value.
    let Expr::BinaryOp { right, .. } = &branches[0].0 else {
        panic!("condition");
    };
    assert!(matches!(right.as_ref(), Expr::Lit(Value::Int(1))));
    assert!(matches!(&branches[0].1, Expr::Lit(Value::Int(5))));
}

// Known names reject a bad parameter count at prepare (MySQL 1582);
// unknown names keep flowing to the runtime "unknown function" error.
#[test]
fn signature_table_rejects_bad_arity_at_translate() {
    for (sql, name) in [
        ("SELECT UPPER('a', 'b')", "upper"),
        ("SELECT ABS()", "abs"),
        ("SELECT VERSION(1)", "version"),
    ] {
        let e = parse_statement(sql).expect_err("arity");
        assert_eq!(e.code, ErrorCode::WrongParamCount, "{sql}: {e}");
        assert!(
            e.msg.contains(&format!(
                "Incorrect parameter count in the call to native function '{name}'"
            )),
            "{sql}: {e}"
        );
    }
    assert!(parse_statement("SELECT NOW(6)").is_ok());
    assert!(parse_statement("SELECT UPPER('a')").is_ok());
    // Arity bounds, table-style: min..=max inclusive.
    assert_eq!(check_signature("upper", 1), Some(Ok(())));
    assert!(check_signature("upper", 0).unwrap().is_err());
    assert!(check_signature("upper", 2).unwrap().is_err());
    assert_eq!(check_signature("now", 1), Some(Ok(())));
    assert_eq!(check_signature("no_such_fn", 1), None);
}

// ---- part B: the new family signatures + special-form translation ----

#[test]
fn part_b_signatures_have_bounds() {
    for (name, ok_argcs, bad_argcs) in [
        ("concat", [1usize, 2, 5].as_slice(), [0].as_slice()),
        ("concat_ws", [2, 3].as_slice(), [0, 1].as_slice()),
        ("substring", [2, 3].as_slice(), [1, 4].as_slice()),
        ("lpad", [3].as_slice(), [2].as_slice()),
        ("trim", [1, 3].as_slice(), [0, 4].as_slice()),
        ("locate", [2, 3].as_slice(), [1].as_slice()),
        ("round", [1, 2].as_slice(), [0, 3].as_slice()),
        ("ceil", [1].as_slice(), [0, 2].as_slice()),
        ("truncate", [2].as_slice(), [1, 3].as_slice()),
        ("greatest", [1, 4].as_slice(), [0].as_slice()),
        ("if", [3].as_slice(), [2, 4].as_slice()),
        ("ifnull", [2].as_slice(), [1, 3].as_slice()),
        ("coalesce", [1, 9].as_slice(), [0].as_slice()),
        ("datediff", [2].as_slice(), [1].as_slice()),
        ("date_format", [2].as_slice(), [1].as_slice()),
        ("unix_timestamp", [0, 1].as_slice(), [2].as_slice()),
        ("from_unixtime", [1, 2].as_slice(), [0, 3].as_slice()),
        ("curtime", [0, 1].as_slice(), [2].as_slice()),
        ("sqrt", [1].as_slice(), [0].as_slice()),
    ] {
        for argc in ok_argcs {
            assert_eq!(check_signature(name, *argc), Some(Ok(())), "{name}({argc})");
        }
        for argc in bad_argcs {
            assert!(
                check_signature(name, *argc).unwrap().is_err(),
                "{name}({argc})"
            );
        }
    }
}

#[test]
fn substring_and_trim_special_forms_translate() {
    // FROM/FOR and comma spellings land on the same Func node.
    for sql in [
        "SELECT SUBSTRING(a FROM 2 FOR 3)",
        "SELECT SUBSTRING(a, 2, 3)",
        "SELECT SUBSTR(a, -2)",
    ] {
        let Expr::Func { name, args } = first_expr(sql) else {
            panic!("{sql}: func");
        };
        assert_eq!(name, "substring", "{sql}");
        assert!(args.len() == 2 || args.len() == 3, "{sql}");
    }
    // TRIM forms: plain 1-arg, extended 3-arg with the mode literal.
    let Expr::Func { name, args } = first_expr("SELECT TRIM(a)") else {
        panic!("trim");
    };
    assert_eq!(name, "trim");
    assert_eq!(args.len(), 1);
    for (sql, mode) in [
        ("SELECT TRIM(BOTH 'x' FROM a)", "BOTH"),
        ("SELECT TRIM(LEADING 'x' FROM a)", "LEADING"),
        ("SELECT TRIM(TRAILING 'x' FROM a)", "TRAILING"),
        ("SELECT TRIM('x' FROM a)", "BOTH"), // no keyword = BOTH
        // Keyword without a remstr: MySQL defaults remstr to ' '.
        ("SELECT TRIM(BOTH FROM a)", "BOTH"),
        ("SELECT TRIM(LEADING FROM a)", "LEADING"),
        ("SELECT TRIM(TRAILING FROM a)", "TRAILING"),
    ] {
        let Expr::Func { name, args } = first_expr(sql) else {
            panic!("{sql}: func");
        };
        assert_eq!(name, "trim", "{sql}");
        assert_eq!(args.len(), 3, "{sql}");
        assert!(
            matches!(&args[2], Expr::Lit(Value::Str(m)) if m == mode),
            "{sql} mode literal"
        );
    }
    // The no-remstr keyword forms carry the single-space default as
    // their remstr literal (space, not empty string).
    for sql in [
        "SELECT TRIM(BOTH FROM a)",
        "SELECT TRIM(LEADING FROM a)",
        "SELECT TRIM(TRAILING FROM a)",
    ] {
        let Expr::Func { args, .. } = first_expr(sql) else {
            panic!("{sql}: func");
        };
        assert!(
            matches!(&args[1], Expr::Lit(Value::Str(r)) if r == " "),
            "{sql} remstr default"
        );
    }
    // POSITION(x IN y) is LOCATE(x, y).
    let Expr::Func { name, args } = first_expr("SELECT POSITION('x' IN a)") else {
        panic!("position");
    };
    assert_eq!(name, "locate");
    assert!(matches!(&args[0], Expr::Lit(Value::Str(_))));
}

#[test]
fn interval_forms_desugar() {
    // DATE_ADD(d, INTERVAL n unit) -> (d, n, unit-literal).
    let Expr::Func { name, args } = first_expr("SELECT DATE_ADD(d, INTERVAL 1 DAY)") else {
        panic!("func");
    };
    assert_eq!(name, "date_add");
    assert_eq!(args.len(), 3);
    assert!(matches!(&args[2], Expr::Lit(Value::Str(u)) if u == "DAY"));
    // The operator form d +/- INTERVAL n unit takes the same path.
    for (sql, name) in [
        ("SELECT d + INTERVAL 1 YEAR", "date_add"),
        ("SELECT d - INTERVAL 2 MINUTE", "date_sub"),
        ("SELECT INTERVAL 1 DAY + d", "date_add"),
    ] {
        let Expr::Func { name: n, args } = first_expr(sql) else {
            panic!("{sql}: func");
        };
        assert_eq!(n, name, "{sql}");
        assert_eq!(args.len(), 3, "{sql}");
    }
    // Plain-days ADDDATE/SUBDATE stay 2-arg.
    let Expr::Func { name, args } = first_expr("SELECT ADDDATE(d, 1)") else {
        panic!("adddate");
    };
    assert_eq!(name, "adddate");
    assert_eq!(args.len(), 2);
    // Unsupported units are loud.
    let e = parse_statement("SELECT DATE_ADD(d, INTERVAL 1 QUARTER)").expect_err("quarter");
    assert!(e.msg.contains("INTERVAL unit"), "{e}");
}

#[test]
fn group_concat_translates_with_separator() {
    let stmt = parse_statement("SELECT GROUP_CONCAT(a SEPARATOR ';')").expect("parse");
    let Statement::Select(q) = stmt else {
        panic!("select");
    };
    let SelectItem::Expr { expr, .. } = q.items.into_iter().next().unwrap() else {
        panic!("item");
    };
    let Expr::Agg {
        func,
        arg,
        distinct,
        sep,
    } = expr
    else {
        panic!("agg");
    };
    assert!(matches!(func, crate::sql::parse::ast::AggFunc::GroupConcat));
    assert!(!distinct);
    assert_eq!(sep.as_deref(), Some(";"));
    assert!(arg.is_some());

    // Default separator; multi-arg form wraps in CONCAT.
    let stmt = parse_statement("SELECT GROUP_CONCAT(a, b)").expect("parse");
    let Statement::Select(q) = stmt else {
        panic!("select");
    };
    let SelectItem::Expr {
        expr: Expr::Agg { func, arg, sep, .. },
        ..
    } = q.items.into_iter().next().unwrap()
    else {
        panic!("agg");
    };
    assert!(matches!(func, crate::sql::parse::ast::AggFunc::GroupConcat));
    assert_eq!(sep, None);
    let Some(Expr::Func { name, args }) = arg.as_deref() else {
        panic!("concat wrap");
    };
    assert_eq!(name, "concat");
    assert_eq!(args.len(), 2);

    // DISTINCT rides the aggregate flag.
    let stmt = parse_statement("SELECT GROUP_CONCAT(DISTINCT a)").expect("parse");
    let Statement::Select(q) = stmt else {
        panic!("select");
    };
    let SelectItem::Expr {
        expr: Expr::Agg { distinct, .. },
        ..
    } = q.items.into_iter().next().unwrap()
    else {
        panic!("agg");
    };
    assert!(distinct);

    // Inner ORDER BY is the loud P2 reject.
    let e = parse_statement("SELECT GROUP_CONCAT(a ORDER BY b)").expect_err("order by");
    assert!(e.msg.contains("GROUP_CONCAT ORDER BY"), "{e}");
}

#[test]
fn lazy_control_calls_translate() {
    for sql in [
        "SELECT IF(a, 1, 2)",
        "SELECT IFNULL(a, 1)",
        "SELECT NULLIF(a, 1)",
        "SELECT COALESCE(a, 1, 2)",
    ] {
        let Expr::Func { .. } = first_expr(sql) else {
            panic!("{sql}: func");
        };
    }
    // Bad arity fails at prepare time (MySQL 1582).
    let e = parse_statement("SELECT IF(a, 1)").expect_err("arity");
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
}
