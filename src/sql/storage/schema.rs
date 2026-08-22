//! SQL table schemas: the catalog payload replicated through raft.
//!
//! Pure data types only; loading/persisting lives in `catalog.rs`, physical
//! row encoding in `row.rs`. Types are deliberately narrowed for v1
//! (no DECIMAL/DATE/TIME): the parser rejects wider SQL types with a clear
//! unsupported error instead of mis-storing them.

use serde::{Deserialize, Serialize};

/// Column value domain of the SQL engine (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlType {
    Bool,
    Int,
    Double,
    VarChar,
    Blob,
}

/// A runtime value. `Null` is its own variant (SQL three-valued logic).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    Bytes(Vec<u8>),
}

impl Value {
    /// SQL type of a non-null value (Null has none; callers decide).
    pub fn sql_type(&self) -> Option<SqlType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(SqlType::Bool),
            Value::Int(_) => Some(SqlType::Int),
            Value::Double(_) => Some(SqlType::Double),
            Value::Str(_) => Some(SqlType::VarChar),
            Value::Bytes(_) => Some(SqlType::Blob),
        }
    }
}

/// One column of a table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    #[serde(rename = "type")]
    pub sql_type: SqlType,
    pub nullable: bool,
}

/// One secondary index. v1 indexes exactly one column.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexDef {
    pub id: u32,
    pub name: String,
    pub column: String,
    pub unique: bool,
}

/// A table schema, stored as JSON under `sql_catalog/<table>` (see
/// `catalog.rs`). `id` is stable across renames (there are none in v1) and
/// namespaces physical row keys, so a dropped+recreated table never reads
/// the old table's orphaned rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableSchema {
    pub id: u32,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Exactly one primary-key column in v1 (enforced at DDL time).
    pub pk: String,
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
}

impl TableSchema {
    /// Column position by (case-insensitive) name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    pub fn column(&self, idx: usize) -> &ColumnDef {
        &self.columns[idx]
    }

    pub fn pk_index(&self) -> usize {
        self.column_index(&self.pk)
            .expect("schema validated at DDL: pk exists")
    }

    /// Storage type of the primary-key column.
    pub fn pk_type(&self) -> SqlType {
        self.columns[self.pk_index()].sql_type
    }

    pub fn index(&self, name: &str) -> Option<&IndexDef> {
        self.indexes
            .iter()
            .find(|i| i.name.eq_ignore_ascii_case(name))
    }

    pub fn index_of_column(&self, column: &str) -> Option<&IndexDef> {
        self.indexes
            .iter()
            .find(|i| i.column.eq_ignore_ascii_case(column))
    }

    /// MySQL `NOT NULL` flag helper for resultset column metadata.
    pub fn mysql_type(&self, idx: usize) -> opensrv_mysql::ColumnType {
        use opensrv_mysql::ColumnType as T;
        match self.columns[idx].sql_type {
            SqlType::Bool => T::MYSQL_TYPE_TINY,
            SqlType::Int => T::MYSQL_TYPE_LONGLONG,
            SqlType::Double => T::MYSQL_TYPE_DOUBLE,
            SqlType::VarChar => T::MYSQL_TYPE_VAR_STRING,
            SqlType::Blob => T::MYSQL_TYPE_BLOB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> TableSchema {
        TableSchema {
            id: 7,
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    sql_type: SqlType::Int,
                    nullable: false,
                },
                ColumnDef {
                    name: "v".into(),
                    sql_type: SqlType::VarChar,
                    nullable: true,
                },
            ],
            pk: "id".into(),
            indexes: vec![],
        }
    }

    #[test]
    fn catalog_json_round_trip() {
        let js = serde_json::to_string(&demo()).expect("ser");
        let back: TableSchema = serde_json::from_str(&js).expect("de");
        assert_eq!(back, demo());
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let s = demo();
        assert_eq!(s.column_index("V"), Some(1));
        assert_eq!(s.pk_index(), 0);
        assert_eq!(s.column_index("nope"), None);
    }
}
