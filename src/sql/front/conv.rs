//! Value conversion between the MySQL wire protocol and the engine's
//! [`Value`] domain, plus resultset column metadata.
//!
//! Parameter direction: prepared-statement [`ParamValue`]s are decoded by
//! opensrv into a small typed enum; [`param_to_value`] narrows it to what
//! the v1 engine stores (no temporal types -- the parser rejects them, so
//! sending one is a client bug, surfaced as an error).
//!
//! Row direction: [`write_value`] feeds each [`Value`] variant to opensrv's
//! `write_col` with a `ToMysqlValue` implementation whose binary-protocol
//! encoder accepts our column type (see [`sql_type_to_mysql`]; the
//! UNSIGNED_FLAG stays unset because the i64 encoder asserts signedness).

use std::io;

use opensrv_mysql::{Column, ColumnFlags, ColumnType, ParamValue, RowWriter, ValueInner};
use tokio::io::AsyncWrite;

use crate::sql::exec::ColMeta;
use crate::sql::storage::schema::{SqlType, Value};

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
        ValueInner::Date(_) | ValueInner::Time(_) | ValueInner::Datetime(_) => Err(format!(
            "DATE/TIME/DATETIME parameters are not supported (column type {:?})",
            coltype
        )),
    }
}

/// Wire type for one engine type. Mirrors `TableSchema::mysql_type`
/// (schema.rs) without reaching into per-table state.
pub fn sql_type_to_mysql(t: SqlType) -> ColumnType {
    match t {
        SqlType::Bool => ColumnType::MYSQL_TYPE_TINY,
        SqlType::Int => ColumnType::MYSQL_TYPE_LONGLONG,
        SqlType::Double => ColumnType::MYSQL_TYPE_DOUBLE,
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
/// signedness assertion matches our empty column flags).
pub fn write_value<W>(w: &mut RowWriter<'_, W>, v: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match v {
        Value::Null => w.write_col(None::<i64>),
        Value::Bool(b) => w.write_col(i8::from(*b)),
        Value::Int(i) => w.write_col(*i),
        Value::Double(f) => w.write_col(*f),
        Value::Str(s) => w.write_col(s.as_str()),
        Value::Bytes(b) => w.write_col(b.as_slice()),
    }
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
    fn temporal_params_are_rejected() {
        for inner in [
            ValueInner::Date(&[0u8; 4]),
            ValueInner::Time(&[0u8; 12]),
            ValueInner::Datetime(&[0u8; 11]),
        ] {
            assert!(param(inner).is_err());
        }
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
