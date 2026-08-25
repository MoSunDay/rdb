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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// Table storage engine. Row = the MVCC row-store in RocksDB (the
/// default); Columnar = append-only versioned segment files + RocksDB
/// segment meta (kind 0x23). Old catalog JSON without the field
/// decodes as Row (`#[serde(default)]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Row,
    Columnar,
}

impl Engine {
    pub fn is_columnar(self) -> bool {
        matches!(self, Engine::Columnar)
    }
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
    /// AUTO_INCREMENT column name, when the table has one (MySQL
    /// server-side id allocation on INSERT; see `exec/sequence.rs`).
    /// Old catalog JSON without the field decodes as `None`.
    #[serde(default)]
    pub auto_increment: Option<String>,
    #[serde(default)]
    pub engine: Engine,
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

    /// Position of the AUTO_INCREMENT column, if any (the name is
    /// validated to exist at DDL time, so this never misses).
    pub fn auto_increment_index(&self) -> Option<usize> {
        self.auto_increment
            .as_ref()
            .and_then(|c| self.column_index(c))
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
            auto_increment: None,
            engine: Engine::Row,
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

    /// Pre-AUTO_INCREMENT catalog JSON (no field) must keep loading as
    /// `None` -- persisted catalogs exist on live clusters.
    #[test]
    fn old_catalog_json_without_auto_increment_loads_none() {
        let js = serde_json::to_string(&demo()).expect("ser");
        // Strip the field the way an old writer's JSON looks.
        let old = js.replace(",\"auto_increment\":null", "");
        let back: TableSchema = serde_json::from_str(&old).expect("de old json");
        assert_eq!(back.auto_increment, None);
        assert_eq!(back, demo());
    }

    #[test]
    fn auto_increment_round_trip_and_index() {
        let mut s = demo();
        s.auto_increment = Some("ID".into());
        let js = serde_json::to_string(&s).expect("ser");
        let back: TableSchema = serde_json::from_str(&js).expect("de");
        assert_eq!(back.auto_increment, Some("ID".into()));
        // Name lookup stays case-insensitive for the AI column too.
        assert_eq!(back.auto_increment_index(), Some(0));
        assert_eq!(demo().auto_increment_index(), None);
    }
}
