//! Result-type table tests: the input-mirroring entries and the lazy
//! control typing.

use super::*;
use crate::sql::parse::ast::Expr;
use crate::sql::storage::schema::Value;

/// Literal-driven typing (the closure resolves literals by value).
fn ty_of(name: &str, args: &[Expr]) -> crate::sql::storage::schema::SqlType {
    result_type(name, args, &|a| match a {
        Expr::Lit(v) => v
            .sql_type()
            .unwrap_or(crate::sql::storage::schema::SqlType::VarChar),
        _ => crate::sql::storage::schema::SqlType::Int,
    })
}

fn lit(v: Value) -> Expr {
    Expr::Lit(v)
}

#[test]
fn mirroring_entries_follow_their_input() {
    use crate::sql::storage::schema::SqlType::*;
    // ROUND: decimal in -> decimal out at the literal d, capped by the
    // argument scale; unknown d keeps the argument scale.
    let d2 = |scale: u8| Decimal {
        precision: 38,
        scale,
    };
    let dec_arg = || lit(Value::Decimal(1, 4));
    assert_eq!(ty_of("round", &[dec_arg(), lit(Value::Int(2))]), d2(2));
    assert_eq!(ty_of("round", &[dec_arg(), lit(Value::Int(9))]), d2(4));
    assert_eq!(ty_of("round", &[dec_arg(), lit(Value::Int(-1))]), d2(0));
    assert_eq!(ty_of("round", &[dec_arg()]), d2(4));
    // ceil/floor drop the scale.
    assert_eq!(ty_of("ceil", &[dec_arg()]), d2(0));
    // Date arithmetic mirrors the input shape.
    let t = |name: &str, arg: SqlType| {
        result_type(
            name,
            &[Expr::Col {
                table: None,
                name: "x".into(),
            }],
            &|_| arg,
        )
    };
    assert_eq!(t("date_add", Date), Date);
    assert_eq!(t("date_sub", DateTime), DateTime);
    assert_eq!(t("adddate", Int), Int);
    assert_eq!(t("adddate", VarChar), VarChar);
    // Extraction answers Int, unhex answers a binary string.
    assert_eq!(ty_of("year", &[lit(Value::Int(0))]), Int);
    assert_eq!(ty_of("unhex", &[lit(Value::Str("ff".into()))]), Blob);
    // Names arrive case-insensitive from the parser.
    assert_eq!(ty_of("YEAR", &[lit(Value::Int(0))]), Int);
}

#[test]
fn lazy_control_and_reducer_typing() {
    use crate::sql::storage::schema::SqlType::*;
    let t = |name: &str, types: &[SqlType]| {
        let cols: Vec<Expr> = types
            .iter()
            .enumerate()
            .map(|(i, _)| Expr::Col {
                table: None,
                name: format!("x{i}"),
            })
            .collect();
        let pick = |e: &Expr| {
            let Expr::Col { name, .. } = e else {
                return Int;
            };
            types[name[1..].parse::<usize>().expect("index")]
        };
        result_type(name, &cols, &pick)
    };
    // Any text branch widens the column to text.
    assert_eq!(t("if", &[Int, VarChar, Int]), VarChar);
    assert_eq!(t("coalesce", &[Int, Double]), Double);
    assert_eq!(t("ifnull", &[Int, Int]), Int);
    assert_eq!(t("greatest", &[Int, Int]), Int);
    assert_eq!(
        t(
            "greatest",
            &[
                Int,
                Decimal {
                    precision: 9,
                    scale: 2
                }
            ]
        ),
        Decimal {
            precision: 38,
            scale: 2
        }
    );
}
