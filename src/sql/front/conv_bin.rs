//! Resultset cell encoding against the ANNOUNCED column type.
//!
//! The engine's static result typing is best-effort (a `?` placeholder
//! types as VarChar until EXECUTE binds a value, CASE types from its
//! first THEN), so a runtime cell can disagree with the column the
//! client was promised. The text protocol is type-blind (every value
//! has a lenenc text spelling), but the BINARY protocol is type-tagged:
//! opensrv's per-type encoders refuse mismatches with an io error,
//! which drops the whole connection -- the mysql-gap M5 open follow-up
//! (`COALESCE(NULL, ?)` + a numeric bind was the repro).
//!
//! Closing that gap (2026-10-08): [`Cell`] encodes every cell IN the
//! announced column's wire form wherever a faithful spelling exists
//! ("compatible coercion" -- e.g. an Int bound into a VAR_STRING
//! placeholder column ships as the text `42`, byte-identical to what
//! the text protocol sends). Combinations with no faithful spelling
//! (a string cell against a numeric column) are rejected loudly
//! BEFORE the resultset starts by [`preflight_binary`]: a 1292 ERR
//! packet instead of a mid-row io error, so the connection survives.

use std::borrow::Cow;
use std::io::{self, Write};

use opensrv_mysql::{Column, ColumnFlags, ColumnType, RowWriter, ToMysqlValue};
use tokio::io::AsyncWrite;

use crate::sql::storage::schema::{format_decimal, Value};
use crate::sql::temporal::{self, MICROS_PER_DAY};

use super::conv::{bad_col, DateCell, DateTimeCell, DecimalCell};

/// One resultset cell: the runtime [`Value`], paired at encode time
/// with the column the client was promised (opensrv's `RowWriter` hands
/// the announced [`Column`] to `to_mysql_bin`). The text form delegates
/// to opensrv's per-type impls, so text-protocol bytes are identical to
/// the pre-coercion wire; only the binary form re-dispatches (below).
struct Cell<'a>(&'a Value);

impl ToMysqlValue for Cell<'_> {
    fn to_mysql_text<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self.0 {
            // The binary form never encodes Null (`RowWriter` rides the
            // null bitmap for `is_null` cells); the TEXT form spells it
            // as the single 0xFB byte, exactly like opensrv's `Option`.
            Value::Null => w.write_all(&[0xfb]),
            Value::Bool(b) => i8::from(*b).to_mysql_text(w),
            Value::Int(i) => i.to_mysql_text(w),
            Value::Double(f) => f.to_mysql_text(w),
            Value::Decimal(m, s) => DecimalCell(*m, *s).to_mysql_text(w),
            Value::Date(d) => DateCell(*d).to_mysql_text(w),
            Value::DateTime(us) => DateTimeCell(*us).to_mysql_text(w),
            Value::Str(s) => s.to_mysql_text(w),
            Value::Bytes(b) => b.as_slice().to_mysql_text(w),
        }
    }

    fn to_mysql_bin<W: Write>(&self, w: &mut W, c: &Column) -> io::Result<()> {
        match c.coltype {
            // MySQL ships every string-ish column (NEWDECIMAL included)
            // as a length-encoded string in the binary protocol; the
            // value's canonical text is a faithful spelling for ANY
            // runtime type -- this is the coercion that keeps a numeric
            // placeholder bind on the wire.
            ColumnType::MYSQL_TYPE_STRING
            | ColumnType::MYSQL_TYPE_VAR_STRING
            | ColumnType::MYSQL_TYPE_VARCHAR
            | ColumnType::MYSQL_TYPE_TINY_BLOB
            | ColumnType::MYSQL_TYPE_BLOB
            | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
            | ColumnType::MYSQL_TYPE_LONG_BLOB
            | ColumnType::MYSQL_TYPE_ENUM
            | ColumnType::MYSQL_TYPE_SET
            | ColumnType::MYSQL_TYPE_BIT
            | ColumnType::MYSQL_TYPE_DECIMAL
            | ColumnType::MYSQL_TYPE_NEWDECIMAL
            | ColumnType::MYSQL_TYPE_JSON
            | ColumnType::MYSQL_TYPE_GEOMETRY => lenenc_put(w, &value_text(self.0)),

            // Integer columns: Int (and Bool as 0/1) width-encode. The
            // engine announces LONGLONG (SqlType::Int) and TINY (Bool);
            // the narrower widths are future-proofing.
            ColumnType::MYSQL_TYPE_LONGLONG
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_YEAR
            | ColumnType::MYSQL_TYPE_TINY => match self.0 {
                Value::Int(i) => write_int_width(w, *i, c.coltype),
                Value::Bool(b) => write_int_width(w, i64::from(*b), c.coltype),
                v => Err(bad_col(v, c)),
            },

            // Floating columns take any numeric runtime value (decimal
            // rides the f64 approximation, mirroring the engine's own
            // Decimal -> Double coercion).
            ColumnType::MYSQL_TYPE_DOUBLE => match numeric_f64(self.0) {
                Some(f) => w.write_all(&f.to_le_bytes()),
                None => Err(bad_col(self.0, c)),
            },
            ColumnType::MYSQL_TYPE_FLOAT => match numeric_f64(self.0) {
                Some(f) => w.write_all(&(f as f32).to_le_bytes()),
                None => Err(bad_col(self.0, c)),
            },

            // Temporal columns keep the typed encoders; the cross forms
            // mirror the engine's Date <-> DateTime coercions (midnight
            // of that day / truncate to the day).
            ColumnType::MYSQL_TYPE_DATE => match self.0 {
                Value::Date(d) => DateCell(*d).to_mysql_bin(w, c),
                Value::DateTime(us) => DateCell(us.div_euclid(MICROS_PER_DAY)).to_mysql_bin(w, c),
                v => Err(bad_col(v, c)),
            },
            ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => match self.0 {
                Value::DateTime(us) => DateTimeCell(*us).to_mysql_bin(w, c),
                Value::Date(d) => match d.checked_mul(MICROS_PER_DAY) {
                    Some(us) => DateTimeCell(us).to_mysql_bin(w, c),
                    None => Err(bad_col(self.0, c)),
                },
                v => Err(bad_col(v, c)),
            },

            _ => Err(bad_col(self.0, c)),
        }
    }

    fn is_null(&self) -> bool {
        matches!(self.0, Value::Null)
    }
}

/// Width-checked little-endian encoding for integer columns; a value
/// that does not fit the announced width is a loud encode error (the
/// engine never announces widths narrower than its i64 Int today).
fn write_int_width<W: Write>(w: &mut W, i: i64, t: ColumnType) -> io::Result<()> {
    let over = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("integer {i} overflows {t:?} column"),
        )
    };
    match t {
        ColumnType::MYSQL_TYPE_LONGLONG => w.write_all(&i.to_le_bytes()),
        ColumnType::MYSQL_TYPE_LONG | ColumnType::MYSQL_TYPE_INT24 => {
            w.write_all(&i32::try_from(i).map_err(|_| over())?.to_le_bytes())
        }
        ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR => {
            w.write_all(&i16::try_from(i).map_err(|_| over())?.to_le_bytes())
        }
        ColumnType::MYSQL_TYPE_TINY => {
            w.write_all(&i8::try_from(i).map_err(|_| over())?.to_le_bytes())
        }
        _ => unreachable!("caller dispatches integer column widths only"),
    }
}

/// Numeric view of a runtime value for floating columns.
fn numeric_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Double(f) => Some(*f),
        Value::Decimal(m, s) => Some(*m as f64 / 10f64.powi(i32::from(*s))),
        _ => None,
    }
}

/// The value's canonical text bytes (the same spellings the text
/// protocol ships): decimal integer for Int, `0`/`1` for Bool (the i8
/// encoder), Rust's shortest round-trip f64 Display, the fixed-point
/// decimal text, the canonical temporal forms, raw bytes for Str/Blob.
fn value_text(v: &Value) -> Cow<'_, [u8]> {
    match v {
        Value::Str(s) => Cow::Borrowed(s.as_bytes()),
        Value::Bytes(b) => Cow::Borrowed(b.as_slice()),
        Value::Bool(b) => Cow::Borrowed(if *b { b"1".as_slice() } else { b"0".as_slice() }),
        Value::Int(i) => Cow::Owned(i.to_string().into_bytes()),
        Value::Double(f) => Cow::Owned(f.to_string().into_bytes()),
        Value::Decimal(m, s) => Cow::Owned(format_decimal(*m, *s).into_bytes()),
        Value::Date(d) => Cow::Owned(temporal::format_date(*d).into_bytes()),
        Value::DateTime(us) => Cow::Owned(temporal::format_datetime(*us).into_bytes()),
        // Null rides the null bitmap; it is never spelled as text.
        Value::Null => Cow::Borrowed(&[]),
    }
}

/// MySQL length-encoded prefix + payload (mysql_common's
/// `WriteMysqlExt` spelling, so long strings keep the 0xfc/0xfd/0xfe
/// forms opensrv itself emits for string columns).
fn lenenc_put<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    let n = bytes.len() as u64;
    if n < 252 {
        w.write_all(&[n as u8])?;
    } else if n < 65_536 {
        w.write_all(&[0xfc])?;
        w.write_all(&(n as u16).to_le_bytes())?;
    } else if n < 16_777_216 {
        w.write_all(&[0xfd])?;
        w.write_all(&(n as u32).to_le_bytes()[..3])?;
    } else {
        w.write_all(&[0xfe])?;
        w.write_all(&n.to_le_bytes())?;
    }
    w.write_all(bytes)
}

/// Write one cell of a resultset row. The announced column reaches the
/// binary encoder through opensrv's `RowWriter` (see [`Cell`]); the
/// text protocol ignores it. Null rides the null bitmap.
pub fn write_value<W>(w: &mut RowWriter<'_, W>, v: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    w.write_col(Cell(v))
}

/// Pre-flight the BINARY encoding of every cell of every row against
/// its announced column. Rows are already materialized when the
/// outcome reaches the wire, so the scan is cheap; any combination
/// without a faithful spelling (string cell against a numeric column,
/// NULL against a NOT_NULL column, ragged arity) fails HERE, before
/// the resultset starts, so the caller answers a loud ERR packet
/// instead of an io error mid-row dropping the connection.
pub fn preflight_binary(rows: &[Vec<Value>], cols: &[Column]) -> io::Result<()> {
    let mut scratch = Vec::new();
    for row in rows {
        if row.len() != cols.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "result row has {} cells for {} columns",
                    row.len(),
                    cols.len()
                ),
            ));
        }
        for (cell, col) in row.iter().zip(cols.iter()) {
            // Null cells never reach an encoder: the null bitmap carries
            // them, unless the column was announced NOT_NULL.
            if matches!(cell, Value::Null) {
                if col.colflags.contains(ColumnFlags::NOT_NULL_FLAG) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("NULL value for NOT NULL column '{}'", col.column),
                    ));
                }
                continue;
            }
            scratch.clear();
            Cell(cell)
                .to_mysql_bin(&mut scratch, col)
                .map_err(|e| io::Error::new(e.kind(), format!("column '{}': {e}", col.column)))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "conv_bin_tests.rs"]
mod tests;
