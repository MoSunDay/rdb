use super::{eval_in_group, Unit};
use crate::sql::exec::scan::{FromScope, ScopeSide};
use crate::sql::parse::ast::{AggFunc, Expr};
use crate::sql::parse::error::ErrorCode;
use crate::sql::storage::schema::{SqlType, Value};

/// One-column scope over `t.v`, typed on demand.
fn scope(t: SqlType) -> FromScope {
    FromScope {
        sides: vec![ScopeSide {
            qualifier: "t".into(),
            table: "t".into(),
            columns: vec!["v".into()],
            types: vec![t],
            nullable: vec![true],
            key_pos: None,
            offset: 0,
        }],
    }
}

fn agg(func: AggFunc, arg: Expr) -> Expr {
    Expr::Agg {
        func,
        arg: Some(Box::new(arg)),
        distinct: false,
    }
}

fn run(
    e: &Expr,
    t: SqlType,
    rows: Vec<Vec<Value>>,
) -> Result<Value, crate::sql::parse::error::SqlError> {
    let u = Unit {
        rep: rows.first().cloned().unwrap_or_else(|| vec![Value::Null]),
        rows,
    };
    eval_in_group(e, &scope(t), &u)
}

// SUM over decimals aligns scales, lifts Ints exactly, skips NULLs and
// stays decimal; one Double coarsens the whole aggregate.
#[test]
fn sum_decimal_aligns_scales_and_lifts_ints() {
    let col = Expr::Col {
        table: None,
        name: "v".into(),
    };
    let d = |m: i128, s: u8| Value::Decimal(m, s);
    let rows = vec![
        vec![d(101, 2)],   // 1.01
        vec![d(2350, 3)],  // 2.350, wider scale rides along
        vec![Value::Null], // skipped
        vec![Value::Int(3)],
    ];
    let eq = SqlType::Decimal {
        precision: 18,
        scale: 3,
    };
    // 1.010 + 2.350 + 3.000 = 6.360
    assert_eq!(
        run(&agg(AggFunc::Sum, col.clone()), eq, rows.clone()),
        Ok(d(6360, 3))
    );
    // AVG reuses div_decimal over the non-NULL count: 6.360/3 -> 2.1200000.
    assert_eq!(run(&agg(AggFunc::Avg, col), eq, rows), Ok(d(21_200_000, 7)));
}

#[test]
fn sum_decimal_overflow_is_loud() {
    let eq = SqlType::Decimal {
        precision: 38,
        scale: 0,
    };
    let rows = vec![vec![Value::Decimal(i128::MAX, 0)], vec![Value::Int(1)]];
    let e = run(
        &agg(
            AggFunc::Sum,
            Expr::Col {
                table: None,
                name: "v".into(),
            },
        ),
        eq,
        rows,
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("SUM overflow"), "{e}");
}

// Mixing a Double into the group coarsens SUM/AVG to Double arithmetic,
// matching the non-aggregate BinaryOp semantics.
#[test]
fn sum_decimal_with_double_coarsens() {
    let eq = SqlType::Double;
    let rows = vec![vec![Value::Decimal(150, 2)], vec![Value::Double(1.0)]];
    let col = Expr::Col {
        table: None,
        name: "v".into(),
    };
    assert_eq!(
        run(&agg(AggFunc::Sum, col.clone()), eq, rows.clone()),
        Ok(Value::Double(2.5))
    );
    assert_eq!(
        run(&agg(AggFunc::Avg, col), eq, rows),
        Ok(Value::Double(1.25))
    );
}

// MIN/MAX/COUNT keep their generic paths and compare decimals exactly.
#[test]
fn min_max_count_over_decimals() {
    let eq = SqlType::Decimal {
        precision: 10,
        scale: 2,
    };
    let d = |m: i128| Value::Decimal(m, 2);
    let rows = vec![vec![d(-500)], vec![Value::Null], vec![d(150)], vec![d(0)]];
    let col = Expr::Col {
        table: None,
        name: "v".into(),
    };
    assert_eq!(
        run(&agg(AggFunc::Min, col.clone()), eq, rows.clone()),
        Ok(d(-500))
    );
    assert_eq!(
        run(&agg(AggFunc::Max, col.clone()), eq, rows.clone()),
        Ok(d(150))
    );
    assert_eq!(run(&agg(AggFunc::Count, col), eq, rows), Ok(Value::Int(3)));
}
