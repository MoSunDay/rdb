//! Unit tests for [`super::conv_bin`] (extracted to keep the module
//! file within the line budget; mirrors the `exec/expr_tests.rs`
//! convention).

use super::*;
use crate::sql::storage::schema::SqlType;

fn col(t: SqlType) -> Column {
    super::super::conv::sql_type_column("c", "t", t)
}

fn bin(v: &Value, c: &Column) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    Cell(v).to_mysql_bin(&mut buf, c)?;
    Ok(buf)
}

/// The mysql-gap open follow-up: numeric (and temporal) runtime
/// cells against a VAR_STRING-typed placeholder column encode as
/// the canonical text -- no io error, no dropped connection.
#[test]
fn numeric_cells_coerce_into_text_columns() {
    let c = col(SqlType::VarChar);
    assert_eq!(bin(&Value::Int(42), &c).unwrap(), b"\x0242".to_vec());
    assert_eq!(bin(&Value::Int(-5), &c).unwrap(), b"\x02-5".to_vec());
    assert_eq!(bin(&Value::Bool(true), &c).unwrap(), b"\x011".to_vec());
    assert_eq!(bin(&Value::Double(1.5), &c).unwrap(), b"\x031.5".to_vec());
    assert_eq!(
        bin(&Value::Decimal(-5, 2), &c).unwrap(),
        b"\x05-0.05".to_vec()
    );
    assert_eq!(
        bin(&Value::Date(19_782), &c).unwrap(),
        b"\x0a2024-02-29".to_vec()
    );
    // Strings/Blobs keep opensrv's own (lenenc) spelling.
    assert_eq!(
        bin(&Value::Str("ab".into()), &c).unwrap(),
        b"\x02ab".to_vec()
    );
    // Long strings keep the 0xfc length form (no <252 regression).
    let long = Value::Str("x".repeat(300));
    let got = bin(&long, &c).unwrap();
    assert_eq!(&got[..3], &[0xfc, 44, 1]);
    assert_eq!(got.len(), 3 + 300);
}

/// Text-protocol bytes are untouched by the coercion: they delegate
/// to opensrv's per-type impls exactly as the old write path did.
#[test]
fn text_protocol_bytes_unchanged() {
    let mut via_cell = Vec::new();
    Cell(&Value::Int(42)).to_mysql_text(&mut via_cell).unwrap();
    let mut direct = Vec::new();
    42i64.to_mysql_text(&mut direct).unwrap();
    assert_eq!(via_cell, direct);

    via_cell.clear();
    Cell(&Value::Str("ab".into()))
        .to_mysql_text(&mut via_cell)
        .unwrap();
    direct.clear();
    "ab".to_mysql_text(&mut direct).unwrap();
    assert_eq!(via_cell, direct);

    // Bool keeps the i8 encoder's text ("0"/"1").
    via_cell.clear();
    Cell(&Value::Bool(false))
        .to_mysql_text(&mut via_cell)
        .unwrap();
    assert_eq!(via_cell, b"\x010".to_vec());

    // Null text cells keep opensrv's single 0xFB byte (the binary form
    // never spells Null -- the null bitmap carries it).
    via_cell.clear();
    Cell(&Value::Null).to_mysql_text(&mut via_cell).unwrap();
    assert_eq!(via_cell, vec![0xfb]);
}

/// Previously-working binary pairs keep their exact bytes: i64 into
/// LONGLONG, Bool into TINY, Decimal into NEWDECIMAL (text form).
#[test]
fn matching_pairs_encode_as_before() {
    assert_eq!(
        bin(&Value::Int(-2), &col(SqlType::Int)).unwrap(),
        (-2i64).to_le_bytes().to_vec()
    );
    assert_eq!(
        bin(&Value::Bool(true), &col(SqlType::Bool)).unwrap(),
        vec![1]
    );
    assert_eq!(
        bin(
            &Value::Decimal(123, 2),
            &col(SqlType::Decimal {
                precision: 10,
                scale: 2
            })
        )
        .unwrap(),
        b"\x041.23".to_vec()
    );
    assert_eq!(
        bin(&Value::Double(1.5), &col(SqlType::Double)).unwrap(),
        1.5f64.to_le_bytes().to_vec()
    );
}

/// Integer widths narrow when they fit and fail loudly when they do
/// not; strings never encode into integer columns (loud, not io
/// panic) -- the preflight turns this into an ERR packet.
#[test]
fn integer_widths_and_mismatches() {
    let long = Column {
        coltype: ColumnType::MYSQL_TYPE_LONG,
        ..col(SqlType::Int)
    };
    let short = Column {
        coltype: ColumnType::MYSQL_TYPE_SHORT,
        ..col(SqlType::Int)
    };
    let tiny = Column {
        coltype: ColumnType::MYSQL_TYPE_TINY,
        ..col(SqlType::Int)
    };
    assert_eq!(
        bin(&Value::Int(700), &long).unwrap(),
        700i32.to_le_bytes().to_vec()
    );
    assert!(bin(&Value::Int(70_000), &short).is_err());
    assert!(bin(&Value::Int(200), &tiny).is_err());
    assert!(bin(&Value::Str("42".into()), &col(SqlType::Int)).is_err());
    assert!(bin(&Value::Str("42".into()), &col(SqlType::Double)).is_err());
}

/// Floating columns take every numeric runtime value.
#[test]
fn float_columns_take_numerics() {
    let dcol = col(SqlType::Double);
    assert_eq!(
        bin(&Value::Int(2), &dcol).unwrap(),
        2f64.to_le_bytes().to_vec()
    );
    assert_eq!(
        bin(&Value::Decimal(15, 1), &dcol).unwrap(),
        1.5f64.to_le_bytes().to_vec()
    );
}

/// Temporal cross-coercion mirrors the engine's Date <-> DateTime
/// semantics (midnight / day truncation).
#[test]
fn temporal_cross_coercion() {
    let midnight = 19_782i64 * MICROS_PER_DAY;
    let mut want = Vec::new();
    DateTimeCell(midnight)
        .to_mysql_bin(&mut want, &col(SqlType::DateTime))
        .unwrap();
    assert_eq!(
        bin(&Value::Date(19_782), &col(SqlType::DateTime)).unwrap(),
        want
    );
    let mut want = Vec::new();
    DateCell(19_782)
        .to_mysql_bin(&mut want, &col(SqlType::Date))
        .unwrap();
    assert_eq!(
        bin(
            &Value::DateTime(midnight + 3_600_000_000),
            &col(SqlType::Date)
        )
        .unwrap(),
        want
    );
}

/// The preflight catches unrepresentable cells (with the column
/// name), NULL against NOT_NULL columns, and ragged arity -- before
/// any resultset bytes go out.
#[test]
fn preflight_catches_unencodable_cells() {
    let cols = vec![col(SqlType::Int), col(SqlType::VarChar)];
    // All-encodable rows pass, NULLs included.
    preflight_binary(
        &[
            vec![Value::Int(1), Value::Null],
            vec![Value::Null, Value::Str("x".into())],
        ],
        &cols,
    )
    .unwrap();

    // A string cell against the integer column fails loudly.
    let err = preflight_binary(&[vec![Value::Str("zz".into()), Value::Null]], &cols).unwrap_err();
    assert!(err.to_string().contains("column 'c'"), "{err}");

    // Arity mismatch.
    let err = preflight_binary(&[vec![Value::Int(1)]], &cols).unwrap_err();
    assert!(err.to_string().contains("1 cells for 2 columns"), "{err}");

    // NULL against an announced NOT NULL column.
    let mut nn = col(SqlType::VarChar);
    nn.colflags |= ColumnFlags::NOT_NULL_FLAG;
    let err = preflight_binary(
        &[vec![Value::Int(1), Value::Null]],
        &[col(SqlType::Int), nn],
    )
    .unwrap_err();
    assert!(err.to_string().contains("NOT NULL"), "{err}");
}
