//! turso <-> rdb value and result-metadata conversion. Every turso
//! result cell crosses `value_from` before the frontend's wire
//! encoder sees it; column metadata prefers the engine's declared
//! type (the honest per-column typing) and falls back to first-value
//! inference for computed columns.

use crate::sql::exec::ColMeta;
use crate::sql::storage::schema::{SqlType, Value};

use super::translate::is_vector_decl;

/// Map one turso result cell onto the rdb value domain. SQLite's
/// dynamic typing is narrower than MySQL's (no separate boolean or
/// decimal), so Int/Real/Text/Blob land directly and NULL stays NULL.
pub fn value_from(v: turso::Value) -> Value {
    match v {
        turso::Value::Null => Value::Null,
        turso::Value::Integer(i) => Value::Int(i),
        turso::Value::Real(f) => Value::Double(f),
        turso::Value::Text(t) => Value::Str(t),
        turso::Value::Blob(b) => Value::Bytes(b),
    }
}

/// [`value_from`] with the column's declared type in hand: a
/// vector-typed column returning a well-formed blob (non-empty,
/// length % 4 == 0 = the Float32Dense raw LE f32 layout) decodes into
/// [`Value::Vector`], which the MySQL layer renders as '[e,e,...]'
/// text. Anything else -- odd-length blobs, TEXT riding a vector
/// column (a row written by an external tool), every non-vector
/// column -- falls back to the plain conversion (defensive: a
/// mis-shaped payload surfaces as bytes, never as a panic or invented
/// values).
pub fn value_from_typed(v: turso::Value, decl: Option<&str>) -> Value {
    if let (Some(d), turso::Value::Blob(b)) = (decl, &v) {
        if is_vector_decl(d) && !b.is_empty() && b.len() % 4 == 0 {
            return Value::Vector(
                b.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c))
                    .collect(),
            );
        }
    }
    value_from(v)
}

/// SQLite declared type -> the rdb column type the MySQL layer
/// announces. `vector(...)` keeps the BLOB face (T4 decision: the
/// host engine announces MYSQL_TYPE_BLOB for vector columns too, and
/// the wire spells vectors as '[e,e,...]' text either way).
fn type_of_decl(decl: Option<&str>) -> Option<SqlType> {
    let d = decl?.trim().to_ascii_uppercase();
    Some(match d.as_str() {
        "INTEGER" => SqlType::Int,
        "REAL" => SqlType::Double,
        "TEXT" => SqlType::VarChar,
        "BLOB" => SqlType::Blob,
        other if other.starts_with("VECTOR") => SqlType::Blob,
        _ => return None,
    })
}

/// Result column metadata for one turso column: computed-column shape
/// (nullable, unkeyed), typed by the declared type when the column
/// has one (a real table column), else from the first non-null value
/// below it, else the safe text default.
pub fn colmeta_from(name: &str, decl: Option<&str>, first: Option<&Value>) -> ColMeta {
    let sql_type = type_of_decl(decl)
        .or_else(|| first.and_then(|v| v.sql_type()))
        .unwrap_or(SqlType::VarChar);
    ColMeta::computed("", name, sql_type)
}
