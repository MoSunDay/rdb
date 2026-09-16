//! SQL table schemas: the catalog payload replicated through raft.
//!
//! Pure data types only; loading/persisting lives in `catalog.rs`, physical
//! row encoding in `row.rs`. Types are deliberately narrowed for v1: the
//! temporal domain is DATE/DATETIME (TIMESTAMP parses as DATETIME) via
//! `temporal`, and DECIMAL stores exact fixed-point `i128` mantissas (the
//! exact-arithmetic executor paths land in a follow-up batch); TIME is
//! still rejected at parse time with a clear unsupported error instead of
//! mis-storing it.

use serde::{Deserialize, Serialize};

/// Max scale of a DECIMAL value (mantissa stays inside `i128`).
pub const MAX_DECIMAL_SCALE: u8 = 38;

/// Column value domain of the SQL engine (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SqlType {
    Bool,
    Int,
    Double,
    /// Exact fixed-point decimal. `scale` digits live behind the point;
    /// `precision` is the declared column width (1..=38, validated at
    /// DDL time) and is metadata only -- stored values are bounded by
    /// the `i128` mantissa, not by precision.
    Decimal {
        precision: u8,
        scale: u8,
    },
    Date,
    DateTime,
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
    /// Fixed-point decimal: mantissa already scaled by `10^scale`
    /// (`1.23` at scale 2 is `123`). `scale <= MAX_DECIMAL_SCALE`.
    Decimal(i128, u8),
    /// Days since 1970-01-01 (see `temporal`).
    Date(i64),
    /// Microseconds since the epoch (see `temporal`).
    DateTime(i64),
    Str(String),
    Bytes(Vec<u8>),
}

/// Render one fixed-point decimal in canonical SQL form: integer digits,
/// a `.` and exactly `scale` fraction digits (zero-padded). `scale 0`
/// has no point, and `-0` never appears (the mantissa carries the
/// sign). Pure formatting, shared by result rendering, literal display
/// and the MySQL wire layer.
pub fn format_decimal(mantissa: i128, scale: u8) -> String {
    if scale == 0 {
        return mantissa.to_string();
    }
    let neg = mantissa < 0;
    let digits = mantissa.unsigned_abs().to_string();
    let scale = scale as usize;
    let int_len = digits.len().saturating_sub(scale);
    let mut out = String::with_capacity(digits.len() + 3);
    if neg {
        out.push('-');
    }
    if int_len == 0 {
        out.push('0');
    } else {
        out.push_str(&digits[..int_len]);
    }
    out.push('.');
    for _ in digits.len()..scale {
        out.push('0');
    }
    out.push_str(&digits[int_len..]);
    out
}

impl Value {
    /// SQL type of a non-null value (Null has none; callers decide).
    /// Decimal reports the nominal maximum precision: a value carries
    /// only its scale, and precision is column metadata (see
    /// [`SqlType::Decimal`]).
    pub fn sql_type(&self) -> Option<SqlType> {
        match self {
            Value::Null => None,
            Value::Bool(_) => Some(SqlType::Bool),
            Value::Int(_) => Some(SqlType::Int),
            Value::Double(_) => Some(SqlType::Double),
            Value::Decimal(_, scale) => Some(SqlType::Decimal {
                precision: MAX_DECIMAL_SCALE,
                scale: *scale,
            }),
            Value::Date(_) => Some(SqlType::Date),
            Value::DateTime(_) => Some(SqlType::DateTime),
            Value::Str(_) => Some(SqlType::VarChar),
            Value::Bytes(_) => Some(SqlType::Blob),
        }
    }

    /// Parse a fixed-point decimal string (`[-]digits[.digits]`) into a
    /// [`Value::Decimal`]: the scale is the fraction digit count and the
    /// mantissa is the digits scaled by `10^scale`. Pure function, shared
    /// by literal coercion and the MySQL wire layer. Errors name the
    /// offender; scale beyond [`MAX_DECIMAL_SCALE`] is rejected.
    pub fn parse_decimal(s: &str) -> Result<Value, String> {
        let body = s
            .strip_prefix('+')
            .or_else(|| s.strip_prefix('-'))
            .unwrap_or(s);
        let neg = s.starts_with('-');
        let (int_part, frac_part) = match body.split_once('.') {
            Some((i, f)) => (i, f),
            None => (body, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(format!("truncated DECIMAL value: '{s}'"));
        }
        if !int_part
            .bytes()
            .chain(frac_part.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return Err(format!("incorrect DECIMAL value: '{s}'"));
        }
        let scale = frac_part.len() as u64;
        if scale > MAX_DECIMAL_SCALE as u64 {
            return Err(format!(
                "DECIMAL scale {} exceeds maximum {}",
                scale, MAX_DECIMAL_SCALE
            ));
        }
        // The digit fold over int+frac parts is already scaled: "12.345"
        // folds to 12345 at scale 3.
        let mantissa: u128 = int_part
            .bytes()
            .chain(frac_part.bytes())
            .fold(0u128, |acc, b| acc * 10 + u128::from(b - b'0'));
        let signed = if neg {
            // `1u128 << 127` has no positive i128 representation; the
            // unsigned fold still reaches it, so map it to i128::MIN.
            if mantissa == 1u128 << 127 {
                Some(i128::MIN)
            } else {
                i128::try_from(mantissa).ok().and_then(|m| m.checked_neg())
            }
        } else {
            i128::try_from(mantissa).ok()
        };
        let mantissa = signed.ok_or_else(|| format!("DECIMAL value out of range: '{s}'"))?;
        Ok(Value::Decimal(mantissa, scale as u8))
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

/// StarRocks table model of a table (Phase 3). `MySql` is the default
/// of pre-Phase-3 catalog JSON (`#[serde(default)]`): plain row-store
/// tables whose INSERT keeps the historical last-write-wins behavior.
/// `PrimaryKey` = StarRocks PK model mapped onto the row engine with
/// upsert INSERT (same pk -> whole-row replace, old index entries
/// cleared). `Duplicate` = StarRocks duplicate model mapped onto the
/// append-only columnar engine (no pk dedup; the first DUPLICATE KEY
/// column is recorded as the schema pk for metadata only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyModel {
    #[default]
    MySql,
    PrimaryKey,
    Duplicate,
}

/// `DISTRIBUTED BY HASH(cols) BUCKETS n` recorded in the schema.
/// Metadata only: rdb's physical distribution stays the engine's own
/// crc16 slot hashing (documented deviation, COMPAT.md) -- no bucket
/// scheduler and no partition pruning runs on these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Distribution {
    pub columns: Vec<String>,
    pub buckets: u32,
}

/// Deserialize `pk`: new catalogs write an array (`["a","b"]`), old
/// catalogs wrote the single-column name as a bare string. Both load;
/// mixed-version rollout depends on exactly this widening (see
/// COMPAT.md).
fn de_string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        One(String),
        Many(Vec<String>),
    }
    match StringOrVec::deserialize(d)? {
        StringOrVec::One(s) => Ok(vec![s]),
        StringOrVec::Many(v) => Ok(v),
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
    /// Primary-key columns in declaration order (one for the classic
    /// single-column pk, more for `PRIMARY KEY(a,b)`; enforced at DDL
    /// time). Serialized as an array; old catalog JSON that carried a
    /// bare string decodes as the one-element vector.
    #[serde(deserialize_with = "de_string_or_vec")]
    pub pk: Vec<String>,

    /// AUTO_INCREMENT column name, when the table has one (MySQL
    /// server-side id allocation on INSERT; see `exec/sequence.rs`).
    /// Old catalog JSON without the field decodes as `None`.
    #[serde(default)]
    pub auto_increment: Option<String>,
    #[serde(default)]
    pub engine: Engine,
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    /// StarRocks table model (see [`KeyModel`]); old catalog JSON
    /// without the field decodes as `MySql` (the pre-Phase-3 engine).
    #[serde(default)]
    pub key_model: KeyModel,
    /// Parsed `DISTRIBUTED BY HASH(...) BUCKETS n`, metadata only.
    #[serde(default)]
    pub distribution: Option<Distribution>,
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

    /// Primary-key column positions in declaration order.
    pub fn pk_indices(&self) -> Vec<usize> {
        self.pk
            .iter()
            .map(|p| {
                self.column_index(p)
                    .expect("schema validated at DDL: every pk column exists")
            })
            .collect()
    }

    /// True for a multi-column `PRIMARY KEY(a,b)`.
    pub fn is_composite_pk(&self) -> bool {
        self.pk.len() > 1
    }

    /// Position of the AUTO_INCREMENT column, if any (the name is
    /// validated to exist at DDL time, so this never misses).
    pub fn auto_increment_index(&self) -> Option<usize> {
        self.auto_increment
            .as_ref()
            .and_then(|c| self.column_index(c))
    }

    /// Storage type of the single primary-key column. Panics on a
    /// composite pk -- callers that must handle both use
    /// [`TableSchema::pk_types`]; a silent first-column pick would
    /// mis-decode every physical key.
    pub fn pk_type(&self) -> SqlType {
        assert!(
            self.pk.len() == 1,
            "pk_type() on a composite pk: use pk_types()"
        );
        self.columns[self.pk_indices()[0]].sql_type
    }

    /// Storage types of all pk columns, in pk order.
    pub fn pk_types(&self) -> Vec<SqlType> {
        self.pk_indices()
            .iter()
            .map(|&i| self.columns[i].sql_type)
            .collect()
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
            SqlType::Decimal { .. } => T::MYSQL_TYPE_NEWDECIMAL,
            SqlType::Date => T::MYSQL_TYPE_DATE,
            SqlType::DateTime => T::MYSQL_TYPE_DATETIME,
            SqlType::VarChar => T::MYSQL_TYPE_VAR_STRING,
            SqlType::Blob => T::MYSQL_TYPE_BLOB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DECIMAL columns/values must round trip through catalog JSON and
    /// keep old (decimal-free) JSON loading: mixed-version clusters
    /// replicate the same payload.
    #[test]
    fn decimal_type_and_value_serde_round_trip() {
        let ty = SqlType::Decimal {
            precision: 12,
            scale: 3,
        };
        let js = serde_json::to_string(&ty).expect("ser");
        assert_eq!(js, r#"{"decimal":{"precision":12,"scale":3}}"#);
        assert_eq!(serde_json::from_str::<SqlType>(&js).unwrap(), ty);
        let v = serde_json::to_value(Value::Decimal(-12345, 3)).expect("ser");
        assert_eq!(
            serde_json::from_value::<Value>(v).unwrap(),
            Value::Decimal(-12345, 3)
        );

        let mut s = demo();
        s.columns.push(ColumnDef {
            name: "amount".into(),
            sql_type: ty,
            nullable: true,
        });
        let js = serde_json::to_string(&s).expect("ser");
        assert!(js.contains(r#""type":{"decimal""#));
        let back: TableSchema = serde_json::from_str(&js).expect("de");
        assert_eq!(back, s);
    }

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
            pk: vec!["id".into()],
            auto_increment: None,
            engine: Engine::Row,
            indexes: vec![],
            key_model: KeyModel::MySql,
            distribution: None,
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
        assert_eq!(s.pk_indices(), vec![0]);
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

    /// Pre-Phase-3 catalog JSON (no key_model/distribution fields)
    /// must keep loading as the MySQL model -- persisted catalogs
    /// exist on live clusters, and mixed-version rollout is gated on
    /// exactly this decode (see COMPAT.md).
    #[test]
    fn old_catalog_json_without_key_model_loads_mysql() {
        let mut s = demo();
        s.key_model = KeyModel::Duplicate;
        s.distribution = Some(Distribution {
            columns: vec!["id".into()],
            buckets: 8,
        });
        let js = serde_json::to_string(&s).expect("ser");
        let old = js
            .replace(",\"key_model\":\"duplicate\"", "")
            .replace(",\"distribution\":{\"columns\":[\"id\"],\"buckets\":8}", "");
        let back: TableSchema = serde_json::from_str(&old).expect("de old json");
        assert_eq!(back.key_model, KeyModel::MySql);
        assert_eq!(back.distribution, None);
    }

    /// Old catalog JSON wrote the pk as a bare string; new JSON writes
    /// an array. The string form must keep decoding as the one-element
    /// pk -- persisted catalogs on live clusters carry it.
    #[test]
    fn old_catalog_string_pk_loads_as_single_column_vec() {
        let js = serde_json::to_string(&demo()).expect("ser");
        assert!(js.contains(r#""pk":["id"]"#));
        let old = js.replace(r#""pk":["id"]"#, r#""pk":"id""#);
        let back: TableSchema = serde_json::from_str(&old).expect("de old json");
        assert_eq!(back.pk, vec!["id".to_string()]);
        assert_eq!(back, demo());
    }

    /// Multi-column pk round trips through catalog JSON unchanged
    /// (order is significant: it is the physical key column order).
    #[test]
    fn composite_pk_json_round_trip() {
        let mut s = demo();
        s.columns.push(ColumnDef {
            name: "day".into(),
            sql_type: SqlType::Date,
            nullable: false,
        });
        s.pk = vec!["day".into(), "id".into()];
        let js = serde_json::to_string(&s).expect("ser");
        assert!(js.contains(r#""pk":["day","id"]"#));
        let back: TableSchema = serde_json::from_str(&js).expect("de");
        assert_eq!(back, s);
        assert_eq!(back.pk_indices(), vec![2, 0]);
        assert!(back.is_composite_pk());
        assert_eq!(back.pk_types(), vec![SqlType::Date, SqlType::Int]);
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

#[cfg(test)]
mod decimal_tests {
    use super::*;

    #[test]
    fn decimal_parse_covers_signs_and_scales() {
        assert_eq!(Value::parse_decimal("0").unwrap(), Value::Decimal(0, 0));
        assert_eq!(
            Value::parse_decimal("-1.23").unwrap(),
            Value::Decimal(-123, 2)
        );
        assert_eq!(
            Value::parse_decimal("+0007.050").unwrap(),
            Value::Decimal(7050, 3)
        );
        assert_eq!(Value::parse_decimal(".5").unwrap(), Value::Decimal(5, 1));
        assert_eq!(Value::parse_decimal("-.5").unwrap(), Value::Decimal(-5, 1));
        assert_eq!(Value::parse_decimal("42.").unwrap(), Value::Decimal(42, 0));
    }

    #[test]
    fn decimal_parse_rejects_malformed() {
        for bad in ["", "+", "-", "1.2.3", "abc", "1e5", " 1", "1 ", "0x10"] {
            assert!(Value::parse_decimal(bad).is_err(), "{bad}");
        }
        // i128::MIN mantissa is in range at scale 0.
        assert_eq!(
            Value::parse_decimal("-170141183460469231731687303715884105728").unwrap(),
            Value::Decimal(i128::MIN, 0)
        );
        // One digit beyond i128 range overflows.
        assert!(Value::parse_decimal("-170141183460469231731687303715884105729").is_err());
        assert!(Value::parse_decimal("170141183460469231731687303715884105728").is_err());
        // Scale cap.
        let deep = format!("0.{}1", "0".repeat(38));
        assert!(Value::parse_decimal(&deep).is_err());
    }

    #[test]
    fn decimal_format_round_trips_parse() {
        for (m, s) in [
            (0i128, 0u8),
            (0, 2),
            (-1, 2),
            (1, 2),
            (123, 2),
            (-123, 2),
            (-5, 9),
            (5, 9),
            (1, 38),
            (-1, 38),
            (i128::MIN, 0),
            (i128::MAX, 3),
        ] {
            let text = format_decimal(m, s);
            assert_eq!(
                Value::parse_decimal(&text).unwrap(),
                Value::Decimal(m, s),
                "{text}"
            );
        }
        assert_eq!(format_decimal(123, 2), "1.23");
        assert_eq!(format_decimal(-123, 2), "-1.23");
        assert_eq!(format_decimal(5, 2), "0.05");
        assert_eq!(format_decimal(-5, 2), "-0.05");
        assert_eq!(format_decimal(120, 2), "1.20");
        assert_eq!(format_decimal(1230, 3), "1.230");
        assert_eq!(format_decimal(7, 0), "7");
        assert_eq!(format_decimal(-7, 0), "-7");
        assert_eq!(format_decimal(0, 4), "0.0000");
    }

    #[test]
    fn decimal_sql_type_reports_nominal_precision() {
        assert_eq!(
            Value::Decimal(-123, 4).sql_type(),
            Some(SqlType::Decimal {
                precision: 38,
                scale: 4
            })
        );
    }
}
