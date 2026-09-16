//! Column typing of CREATE TABLE: sqlparser data types and column
//! options to the engine's narrow `SqlType`/`ColumnSpec` IR (split out
//! of `translate.rs` to keep file sizes in budget).
//!
//! Anything the engine cannot store is rejected here with a loud
//! unsupported error naming the accepted set, so the executor only ever
//! sees representable shapes.

use sqlparser::ast::{ColumnDef as SqlColumnDef, ColumnOption, DataType, ExactNumberInfo};

use crate::sql::parse::ast::ColumnSpec;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::schema::SqlType;

/// One column of a CREATE TABLE: the typed spec plus whether the body
/// declared it the (inline) primary key.
pub fn translate_column(col: &SqlColumnDef) -> SqlResult<(ColumnSpec, bool)> {
    let sql_type = translate_type(&col.data_type)?;
    let mut nullable = true;
    let mut pk = false;
    let mut auto_increment = false;
    for opt in &col.options {
        match &opt.option {
            ColumnOption::Null => nullable = true,
            ColumnOption::NotNull => nullable = false,
            ColumnOption::PrimaryKey(_) => {
                pk = true;
                nullable = false;
            }
            ColumnOption::Unique(_) => {} // use CREATE UNIQUE INDEX instead
            ColumnOption::Default(_) => {} // defaults are client-evaluated in v1
            ColumnOption::Comment(_) => {}
            ColumnOption::DialectSpecific(_)
                if opt.to_string().eq_ignore_ascii_case("AUTO_INCREMENT") =>
            {
                auto_increment = true;
            }
            _ => {
                return Err(SqlError::unsupported(format!(
                    "column option {}",
                    opt.option
                )))
            }
        }
    }
    Ok((
        ColumnSpec {
            name: col.name.value.clone(),
            sql_type,
            nullable,
            auto_increment,
        },
        pk,
    ))
}

pub fn translate_type(t: &DataType) -> SqlResult<SqlType> {
    use DataType::*;
    Ok(match t {
        Bool | Boolean => SqlType::Bool,
        TinyInt(_) | Int2(_) | SmallInt(_) | MediumInt(_) | Int(_) | Int4(_) | Integer(_)
        | Int8(_) | BigInt(_) => SqlType::Int,
        Float(_) | Float4 | Real | Double(_) | Float8 | DoublePrecision => SqlType::Double,
        Varchar(_) | CharVarying(_) | Char(_) | Character(_) | CharacterVarying(_) | Text
        | TinyText | MediumText | LongText | String(_) => SqlType::VarChar,
        Varbinary(_) | Binary(_) | Blob(_) | TinyBlob | MediumBlob | LongBlob | Bytea => {
            SqlType::Blob
        }
        // DATE is days since the epoch, DATETIME microseconds (see
        // `temporal`); TIMESTAMP parses as DATETIME. The optional fsp
        // is ignored -- storage is always microsecond precision.
        Date => SqlType::Date,
        Datetime(_) | Timestamp(_, _) => SqlType::DateTime,
        // DECIMAL/DEC/NUMERIC share one exact fixed-point type. A bare
        // DECIMAL is MySQL's DECIMAL(10,0); precision must fit the i128
        // mantissa (<= 38 digits) and scale must not exceed precision.
        Decimal(info) | Dec(info) | Numeric(info) => decimal_type(info)?,
        other => {
            return Err(SqlError::unsupported(format!(
                "column type {other} (v1: BOOL/INT/DOUBLE/DECIMAL/VARCHAR/BLOB/DATE/DATETIME/TIMESTAMP)"
            )))
        }
    })
}

/// DECIMAL width/scale: bare = (10,0); precision 1..=38, scale 0..=precision.
fn decimal_type(info: &ExactNumberInfo) -> SqlResult<SqlType> {
    let (precision, scale) = match info {
        ExactNumberInfo::None => (10i64, 0i64),
        ExactNumberInfo::Precision(p) => (*p as i64, 0),
        ExactNumberInfo::PrecisionAndScale(p, s) => (*p as i64, *s),
    };
    if !(1..=38).contains(&precision) {
        return Err(SqlError::unsupported(format!(
            "DECIMAL precision {precision} out of range 1..=38"
        )));
    }
    if !(0..=precision).contains(&scale) {
        return Err(SqlError::unsupported(format!(
            "DECIMAL scale {scale} out of range 0..={precision}"
        )));
    }
    Ok(SqlType::Decimal {
        precision: precision as u8,
        scale: scale as u8,
    })
}
