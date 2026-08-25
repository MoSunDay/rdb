//! Value conversion between the MySQL wire protocol and the engine's
//! [`Value`] domain, plus resultset column metadata.
//!
//! Parameter direction: prepared-statement [`ParamValue`]s are decoded by
//! opensrv into a small typed enum; [`param_to_value`] narrows it to what
//! the engine stores. DATE/DATETIME parameters arrive as the binary
//! protocol payloads (length byte already consumed by opensrv) and are
//! decoded into the engine's day/microsecond integers; the zero date has
//! no civil spelling here and TIME has no representation at all, so both
//! are loud rejections.
//!
//! Row direction: [`write_value`] feeds each [`Value`] variant to opensrv's
//! `write_col`. Temporal cells ride the local [`DateCell`]/[`DateTimeCell`]
//! newtypes (foreign trait, local type) so the binary protocol sees the
//! typed encoders for the column type (see [`sql_type_to_mysql`]; the
//! UNSIGNED_FLAG stays unset because the i64 encoder asserts signedness).

use std::io::{self, Write};

use opensrv_mysql::{
    Column, ColumnFlags, ColumnType, ParamValue, RowWriter, ToMysqlValue, ValueInner,
};
use tokio::io::AsyncWrite;

use crate::sql::exec::ColMeta;
use crate::sql::storage::schema::{SqlType, Value};
use crate::sql::temporal::{self, MICROS_PER_DAY};

/// Engine type of one decoded parameter, or a human-readable rejection
/// reason for the temporal types the engine cannot store.
pub fn param_to_value(pv: &ParamValue) -> Result<Value, String> {
    inner_to_value(pv.value.into_inner(), pv.coltype)
}

/// Core mapping, split out so every [`ValueInner`] variant is directly
/// constructible in unit tests (`ParamValue` has no public constructor).
fn inner_to_value(inner: ValueInner<'_>, coltype: ColumnType) -> Result<Value, String> {
    match inner {
        ValueInner::NULL => Ok(Value::Null),
        ValueInner::Int(i) => Ok(Value::Int(i)),
        ValueInner::UInt(u) => {
            // BIGINT UNSIGNED above i64::MAX loses exactness; degrade to
            // DOUBLE rather than wrapping (mirrors MySQL's own lossy cast).
            if u <= i64::MAX as u64 {
                Ok(Value::Int(u as i64))
            } else {
                Ok(Value::Double(u as f64))
            }
        }
        ValueInner::Double(f) => Ok(Value::Double(f)),
        ValueInner::Bytes(b) => match std::str::from_utf8(b) {
            Ok(s) => Ok(Value::Str(s.to_string())),
            Err(_) => Ok(Value::Bytes(b.to_vec())),
        },
        // opensrv already consumed the length byte; the payloads are the
        // date/datetime binary forms (see opensrv value/decode.rs).
        ValueInner::Date(b) => date_param(b),
        ValueInner::Datetime(b) => datetime_param(b),
        // TIME has no engine representation; keep the rejection loud.
        ValueInner::Time(_) => Err(format!(
            "TIME parameters are not supported (column type {:?})",
            coltype
        )),
    }
}

/// The MySQL zero date ('0000-00-00') has no year-0001..=9999 civil
/// spelling in the engine's model, so it cannot be stored faithfully.
const ZERO_DATE_PARAM: &str = "zero DATE/DATETIME parameters are not supported";

/// Binary DATE parameter: 0 bytes (the zero date) or 4 bytes
/// `u16 year LE, u8 month, u8 day`.
fn date_param(b: &[u8]) -> Result<Value, String> {
    let ymd = ymd_param(b, "DATE")?;
    match b.len() {
        0 => Err(ZERO_DATE_PARAM.to_string()),
        4 => temporal::days_from_civil(ymd.0, ymd.1, ymd.2)
            .map(Value::Date)
            .ok_or_else(|| incorrect_param("DATE", ymd)),
        n => Err(format!("malformed DATE parameter: {n} bytes")),
    }
}

/// Binary DATETIME/TIMESTAMP parameter: the y/m/d prefix (4 bytes),
/// optionally followed by `h, mi, s` (7 bytes) and `u32 micros LE`
/// (11 bytes); 0 bytes is the zero date.
fn datetime_param(b: &[u8]) -> Result<Value, String> {
    let (y, m, d) = ymd_param(b, "DATETIME")?;
    let (secs, micros) = match b.len() {
        0 => return Err(ZERO_DATE_PARAM.to_string()),
        4 => (0, 0),
        7 => (hms_param(b)?, 0),
        11 => (
            hms_param(b)?,
            u32::from_le_bytes(b[7..11].try_into().expect("len checked")),
        ),
        n => return Err(format!("malformed DATETIME parameter: {n} bytes")),
    };
    let days =
        temporal::days_from_civil(y, m, d).ok_or_else(|| incorrect_param("DATETIME", (y, m, d)))?;
    Ok(Value::DateTime(
        days * MICROS_PER_DAY + secs * 1_000_000 + i64::from(micros),
    ))
}

/// `u16 year LE, u8 month, u8 day` at the head of a temporal parameter
/// (all-zero at length 0, the zero date).
fn ymd_param(b: &[u8], domain: &str) -> Result<(i64, u32, u32), String> {
    if b.is_empty() {
        return Ok((0, 0, 0));
    }
    let head: [u8; 4] = b
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| format!("malformed {domain} parameter: {} bytes", b.len()))?;
    Ok((
        i64::from(u16::from_le_bytes([head[0], head[1]])),
        u32::from(head[2]),
        u32::from(head[3]),
    ))
}

/// Seconds-of-day of the `h, mi, s` bytes at 4..=6 of a 7/11-byte
/// temporal parameter (None-equivalent becomes a loud rejection).
fn hms_param(b: &[u8]) -> Result<i64, String> {
    let (h, m, s) = (b[4], b[5], b[6]);
    if h > 23 || m > 59 || s > 59 {
        return Err(format!("Incorrect DATETIME value: {h:02}:{m:02}:{s:02}"));
    }
    Ok(i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s))
}

fn incorrect_param(domain: &str, (y, m, d): (i64, u32, u32)) -> String {
    format!("Incorrect {domain} value: '{y:04}-{m:02}-{d:02}'")
}

/// Wire type for one engine type. Mirrors `TableSchema::mysql_type`
/// (schema.rs) without reaching into per-table state.
pub fn sql_type_to_mysql(t: SqlType) -> ColumnType {
    match t {
        SqlType::Bool => ColumnType::MYSQL_TYPE_TINY,
        SqlType::Int => ColumnType::MYSQL_TYPE_LONGLONG,
        SqlType::Double => ColumnType::MYSQL_TYPE_DOUBLE,
        SqlType::Date => ColumnType::MYSQL_TYPE_DATE,
        SqlType::DateTime => ColumnType::MYSQL_TYPE_DATETIME,
        SqlType::VarChar => ColumnType::MYSQL_TYPE_VAR_STRING,
        SqlType::Blob => ColumnType::MYSQL_TYPE_BLOB,
    }
}

/// One resultset column descriptor: engine type -> wire type, no flags
/// (nullability is dynamic in the engine, so NOT_NULL stays unset).
pub fn sql_type_column(name: &str, table: &str, t: SqlType) -> Column {
    Column {
        table: table.to_string(),
        column: name.to_string(),
        coltype: sql_type_to_mysql(t),
        colflags: ColumnFlags::empty(),
    }
}

/// Resultset descriptors for an executor `ColMeta` list.
pub fn colmetas_to_columns(cols: &[ColMeta]) -> Vec<Column> {
    cols.iter()
        .map(|c| sql_type_column(&c.name, &c.table, c.sql_type))
        .collect()
}

/// Placeholder-parameter descriptors (`?` markers have no declared type
/// until EXECUTE supplies values).
pub fn placeholder_columns(n: usize) -> Vec<Column> {
    (0..n)
        .map(|_| sql_type_column("?", "", SqlType::VarChar))
        .collect()
}

/// Write one cell of a resultset row.
///
/// Null rides on `Option`'s `is_null` (null bitmap; never touches the
/// per-type encoder), Bool goes out as a signed TINY 0/1 (the `bool`
/// `ToMysqlValue` impl does not exist; `i8` is the TINY encoder whose
/// signedness assertion matches our empty column flags), and temporal
/// cells ride the typed local encoders below.
pub fn write_value<W>(w: &mut RowWriter<'_, W>, v: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match v {
        Value::Null => w.write_col(None::<i64>),
        Value::Bool(b) => w.write_col(i8::from(*b)),
        Value::Int(i) => w.write_col(*i),
        Value::Double(f) => w.write_col(*f),
        Value::Date(d) => w.write_col(DateCell(*d)),
        Value::DateTime(us) => w.write_col(DateTimeCell(*us)),
        Value::Str(s) => w.write_col(s.as_str()),
        Value::Bytes(b) => w.write_col(b.as_slice()),
    }
}

/// One DATE cell: days since 1970-01-01. Text protocol emits the
/// canonical `YYYY-MM-DD`; binary requires a MYSQL_TYPE_DATE column
/// (mirrors opensrv's own chrono `NaiveDate` impl).
#[derive(Debug)]
struct DateCell(i64);

impl ToMysqlValue for DateCell {
    fn to_mysql_text<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_lenenc_text(w, temporal::format_date(self.0).as_bytes())
    }

    fn to_mysql_bin<W: Write>(&self, w: &mut W, c: &Column) -> io::Result<()> {
        match c.coltype {
            ColumnType::MYSQL_TYPE_DATE => {
                let Some((y, m, d)) = temporal::civil_from_days(self.0) else {
                    return Err(bad_col(self, c));
                };
                w.write_all(&[4u8])?;
                w.write_all(&(y as u16).to_le_bytes())?;
                w.write_all(&[m as u8, d as u8])
            }
            _ => Err(bad_col(self, c)),
        }
    }
}

/// One DATETIME/TIMESTAMP cell: microseconds since the epoch. Fraction
/// is omitted from both wire forms when zero (opensrv's `NaiveDateTime`
/// behavior).
#[derive(Debug)]
struct DateTimeCell(i64);

impl ToMysqlValue for DateTimeCell {
    fn to_mysql_text<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_lenenc_text(w, temporal::format_datetime(self.0).as_bytes())
    }

    fn to_mysql_bin<W: Write>(&self, w: &mut W, c: &Column) -> io::Result<()> {
        match c.coltype {
            ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_TIMESTAMP => {
                let days = self.0.div_euclid(MICROS_PER_DAY);
                let rem = self.0.rem_euclid(MICROS_PER_DAY);
                let (secs, micros) = (rem / 1_000_000, rem % 1_000_000);
                let Some((y, m, d)) = temporal::civil_from_days(days) else {
                    return Err(bad_col(self, c));
                };
                w.write_all(&[if micros == 0 { 7u8 } else { 11u8 }])?;
                w.write_all(&(y as u16).to_le_bytes())?;
                w.write_all(&[
                    m as u8,
                    d as u8,
                    (secs / 3600) as u8,
                    ((secs / 60) % 60) as u8,
                    (secs % 60) as u8,
                ])?;
                if micros != 0 {
                    w.write_all(&(micros as u32).to_le_bytes())?;
                }
                Ok(())
            }
            _ => Err(bad_col(self, c)),
        }
    }
}

/// Length-prefixed text cell: one length byte + payload. Temporal
/// spellings are at most 26 bytes, well inside the one-byte lenenc
/// range (< 252), which is exactly what opensrv's `write_lenenc_str`
/// emits for our sizes.
fn write_lenenc_text<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() >= 252 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporal text cell exceeds one-byte lenenc length",
        ));
    }
    w.write_all(&[bytes.len() as u8])?;
    w.write_all(bytes)
}

/// opensrv's own value/column-type mismatch error style (encode.rs).
fn bad_col(v: &impl std::fmt::Debug, c: &Column) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("tried to use {v:?} as {:?}", c.coltype),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param(inner: ValueInner<'_>) -> Result<Value, String> {
        inner_to_value(inner, ColumnType::MYSQL_TYPE_LONGLONG)
    }

    #[test]
    fn params_map_to_engine_values() {
        assert_eq!(param(ValueInner::NULL), Ok(Value::Null));
        assert_eq!(param(ValueInner::Int(-5)), Ok(Value::Int(-5)));
        assert_eq!(param(ValueInner::UInt(7)), Ok(Value::Int(7)));
        assert_eq!(param(ValueInner::Double(1.5)), Ok(Value::Double(1.5)));
        assert_eq!(
            param(ValueInner::Bytes(b"txt".as_slice())),
            Ok(Value::Str("txt".to_string()))
        );
        // Invalid UTF-8 stays binary instead of being lossily coerced.
        assert_eq!(
            param(ValueInner::Bytes(&[0xff, 0xfe])),
            Ok(Value::Bytes(vec![0xff, 0xfe]))
        );
    }

    #[test]
    fn u64_above_i64_max_degrades_to_double() {
        let big = u64::MAX;
        assert_eq!(param(ValueInner::UInt(big)), Ok(Value::Double(big as f64)));
        assert_eq!(
            param(ValueInner::UInt(i64::MAX as u64 + 1)),
            Ok(Value::Double((i64::MAX as u64 + 1) as f64))
        );
        // The boundary itself stays exact.
        assert_eq!(
            param(ValueInner::UInt(i64::MAX as u64)),
            Ok(Value::Int(i64::MAX))
        );
    }

    #[test]
    fn date_params_decode_to_days() {
        // 2024-02-29: u16 LE year, month, day.
        assert_eq!(
            param(ValueInner::Date(&[0xe8, 0x07, 2, 29])),
            Ok(Value::Date(19_782))
        );
        // zero date (length byte 0) is unrepresentable and loud.
        let err = param(ValueInner::Date(&[])).unwrap_err();
        assert!(err.contains("zero DATE/DATETIME"), "{err}");
        // truncations and unexpected payload lengths are malformed.
        assert!(param(ValueInner::Date(&[0xe8, 0x07, 2]))
            .unwrap_err()
            .contains("malformed"));
        assert!(param(ValueInner::Date(&[0xe8, 0x07, 2, 29, 0]))
            .unwrap_err()
            .contains("malformed"));
        // civil-invalid date (Feb 30) is an incorrect value.
        assert!(param(ValueInner::Date(&[0xe8, 0x07, 2, 30]))
            .unwrap_err()
            .contains("Incorrect DATE value"));
    }

    #[test]
    fn datetime_params_decode_to_micros() {
        // 2024-02-29 01:02:03.123456: ymd + hms + u32 LE micros.
        let full = [0xe8, 0x07, 2, 29, 1, 2, 3, 0x40, 0xe2, 0x01, 0x00];
        assert_eq!(
            param(ValueInner::Datetime(&full)),
            Ok(Value::DateTime(1_709_168_523_123_456))
        );
        // 7-byte form (no fraction).
        assert_eq!(
            param(ValueInner::Datetime(&[0xe8, 0x07, 2, 29, 1, 2, 3])),
            Ok(Value::DateTime(1_709_168_523_000_000))
        );
        // 4-byte form is midnight.
        assert_eq!(
            param(ValueInner::Datetime(&[0xe8, 0x07, 2, 29])),
            Ok(Value::DateTime(1_709_164_800_000_000))
        );
        // zero datetime, bad clock and odd lengths reject loudly.
        assert!(param(ValueInner::Datetime(&[]))
            .unwrap_err()
            .contains("zero DATE/DATETIME"));
        assert!(param(ValueInner::Datetime(&[0xe8, 0x07, 2, 29, 24, 0, 0]))
            .unwrap_err()
            .contains("Incorrect DATETIME value"));
        assert!(param(ValueInner::Datetime(&[0xe8, 0x07, 2, 29, 1]))
            .unwrap_err()
            .contains("malformed"));
    }

    #[test]
    fn time_params_stay_rejected() {
        let err = param(ValueInner::Time(&[0u8; 12])).unwrap_err();
        assert!(err.contains("TIME"), "{err}");
        assert!(!err.contains("DATETIME parameters"), "{err}");
    }

    #[test]
    fn temporal_cells_encode_both_protocols() {
        let dcol = sql_type_column("d", "t", SqlType::Date);
        let tcol = sql_type_column("t", "t", SqlType::DateTime);
        let scol = sql_type_column("s", "t", SqlType::VarChar);

        // text: lenenc-prefixed canonical spellings
        let mut buf = Vec::new();
        DateCell(19_782).to_mysql_text(&mut buf).unwrap();
        assert_eq!(buf, b"\x0a2024-02-29".to_vec());
        buf.clear();
        DateTimeCell(1_709_168_523_123_456)
            .to_mysql_text(&mut buf)
            .unwrap();
        assert_eq!(buf, b"\x1a2024-02-29 01:02:03.123456".to_vec());
        buf.clear();
        DateTimeCell(1_709_164_800_000_000)
            .to_mysql_text(&mut buf)
            .unwrap();
        assert_eq!(buf, b"\x132024-02-29 00:00:00".to_vec());

        // binary: date = 4-byte form; datetime = 7/11 by fraction
        buf.clear();
        DateCell(19_782).to_mysql_bin(&mut buf, &dcol).unwrap();
        assert_eq!(buf, vec![4, 0xe8, 0x07, 2, 29]);
        buf.clear();
        DateTimeCell(1_709_164_800_000_000)
            .to_mysql_bin(&mut buf, &tcol)
            .unwrap();
        assert_eq!(buf, vec![7, 0xe8, 0x07, 2, 29, 0, 0, 0]);
        buf.clear();
        DateTimeCell(1_709_168_523_123_456)
            .to_mysql_bin(&mut buf, &tcol)
            .unwrap();
        assert_eq!(
            buf,
            vec![11, 0xe8, 0x07, 2, 29, 1, 2, 3, 0x40, 0xe2, 0x01, 0x00]
        );

        // column-type mismatches and out-of-range day counts error.
        assert!(DateCell(0).to_mysql_bin(&mut Vec::new(), &tcol).is_err());
        assert!(DateTimeCell(0)
            .to_mysql_bin(&mut Vec::new(), &dcol)
            .is_err());
        assert!(DateCell(i64::MAX)
            .to_mysql_bin(&mut Vec::new(), &dcol)
            .is_err());
        assert!(DateTimeCell(i64::MIN)
            .to_mysql_bin(&mut Vec::new(), &tcol)
            .is_err());
        assert!(DateTimeCell(0)
            .to_mysql_bin(&mut Vec::new(), &scol)
            .is_err());
    }

    #[test]
    fn columns_mirror_schema_mapping() {
        // Same per-type expectations as TableSchema::mysql_type.
        let cols = [
            (SqlType::Bool, ColumnType::MYSQL_TYPE_TINY),
            (SqlType::Int, ColumnType::MYSQL_TYPE_LONGLONG),
            (SqlType::Double, ColumnType::MYSQL_TYPE_DOUBLE),
            (SqlType::VarChar, ColumnType::MYSQL_TYPE_VAR_STRING),
            (SqlType::Blob, ColumnType::MYSQL_TYPE_BLOB),
        ];
        for (t, want) in cols {
            let c = sql_type_column("n", "t", t);
            assert_eq!(c.coltype, want);
            assert_eq!(c.column, "n");
            assert_eq!(c.table, "t");
            assert_eq!(c.colflags, ColumnFlags::empty());
        }
    }

    #[test]
    fn colmetas_build_columns_in_order() {
        let metas = vec![
            ColMeta {
                table: "users".into(),
                name: "id".into(),
                sql_type: SqlType::Int,
            },
            ColMeta {
                table: "".into(),
                name: "count(*)".into(),
                sql_type: SqlType::Double,
            },
        ];
        let cols = colmetas_to_columns(&metas);
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].table, "users");
        assert_eq!(cols[1].table, "");
        assert_eq!(cols[1].coltype, ColumnType::MYSQL_TYPE_DOUBLE);
    }

    #[test]
    fn placeholder_columns_are_untyped_markers() {
        let cols = placeholder_columns(2);
        assert_eq!(cols.len(), 2);
        assert!(cols.iter().all(|c| c.column == "?"));
    }
}
