//! Round-trip + corruption tests for the segment format (M1 scope).

use crate::sql::columnar::decode;
use crate::sql::columnar::encode::{build_segment, ColumnZone, ENC_DICT, ENC_PLAIN};
use crate::sql::columnar::format::{self, crc32, MAGIC};
use crate::sql::storage::schema::{ColumnDef, Engine, SqlType, TableSchema, Value};

fn col(name: &str, sql_type: SqlType, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        sql_type,
        nullable,
    }
}

fn all_types_schema() -> TableSchema {
    TableSchema {
        id: 7,
        name: "t".into(),
        columns: vec![
            col("id", SqlType::Int, false),
            col("b", SqlType::Bool, true),
            col("d", SqlType::Double, true),
            col("s", SqlType::VarChar, true),
            col("x", SqlType::Blob, true),
        ],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Row,
        indexes: vec![],
    }
}

fn int_schema() -> TableSchema {
    TableSchema {
        id: 8,
        name: "ints".into(),
        columns: vec![col("id", SqlType::Int, false)],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Row,
        indexes: vec![],
    }
}

fn str_schema(nullable: bool) -> TableSchema {
    TableSchema {
        id: 9,
        name: "strs".into(),
        columns: vec![
            col("id", SqlType::Int, false),
            col("s", SqlType::VarChar, nullable),
        ],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Row,
        indexes: vec![],
    }
}

#[test]
fn crc32_known_vector() {
    assert_eq!(crc32(b"123456789"), 0xCBF43926);
}

#[test]
fn round_trip_all_types_with_nulls() {
    let schema = all_types_schema();
    let rows: Vec<Vec<Value>> = vec![
        vec![
            Value::Int(1),
            Value::Bool(true),
            Value::Double(1.5),
            Value::Str("hello".into()),
            Value::Bytes(vec![0x01, 0x02]),
        ],
        vec![
            Value::Int(2),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
        vec![
            Value::Int(3),
            Value::Bool(false),
            Value::Double(-2.25),
            Value::Str("world".into()),
            Value::Bytes(vec![]),
        ],
    ];
    let (file, zones) = build_segment(&schema, &rows).unwrap();
    let (footer, region) = decode::open(&file).unwrap();
    assert_eq!(footer.version, format::FOOTER_VERSION);
    assert_eq!(footer.num_rows, 3);
    assert_eq!(footer.columns.len(), 5);
    assert!(!region.is_empty());
    for (ci, col) in schema.columns.iter().enumerate() {
        let expected: Vec<Value> = rows.iter().map(|r| r[ci].clone()).collect();
        assert_eq!(
            decode::decode_column(&file, &footer.columns[ci], col.sql_type).unwrap(),
            expected,
            "column {}",
            col.name
        );
    }
    // Column-level zonemaps.
    assert_eq!(zones.len(), 5);
    assert_eq!(zones[0].min, Value::Int(1));
    assert_eq!(zones[0].max, Value::Int(3));
    assert_eq!(zones[0].null_count, 0);
    assert_eq!(zones[1].null_count, 1);
    assert_eq!(zones[1].min, Value::Bool(false));
    assert_eq!(zones[1].max, Value::Bool(true));
    assert_eq!(zones[3].min, Value::Str("hello".into()));
    assert_eq!(zones[3].max, Value::Str("world".into()));
    assert_eq!(zones[4].min, Value::Bytes(vec![]));
    assert_eq!(zones[4].max, Value::Bytes(vec![0x01, 0x02]));
}

#[test]
fn empty_segment_is_legal() {
    let schema = all_types_schema();
    let (file, zones) = build_segment(&schema, &[]).unwrap();
    let (footer, region) = decode::open(&file).unwrap();
    assert_eq!(footer.num_rows, 0);
    assert!(region.is_empty());
    assert!(footer.columns.iter().all(|c| c.pages.is_empty()));
    for z in &zones {
        assert_eq!(z.null_count, 0);
        assert_eq!(z.min, Value::Null);
        assert_eq!(z.max, Value::Null);
    }
    for (ci, col) in schema.columns.iter().enumerate() {
        assert!(
            decode::decode_column(&file, &footer.columns[ci], col.sql_type)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn single_row_round_trip() {
    let schema = str_schema(true);
    let rows = vec![vec![Value::Int(42), Value::Null]];
    let (file, zones) = build_segment(&schema, &rows).unwrap();
    let (footer, _) = decode::open(&file).unwrap();
    assert_eq!(footer.num_rows, 1);
    assert_eq!(
        decode::decode_column(&file, &footer.columns[1], SqlType::VarChar).unwrap(),
        vec![Value::Null]
    );
    assert_eq!(zones[1].null_count, 1);
    assert_eq!(zones[1].min, Value::Null);
    assert_eq!(zones[1].max, Value::Null);
}

#[test]
fn temporal_columns_round_trip_plain_with_zonemap() {
    let schema = TableSchema {
        id: 10,
        name: "temps".into(),
        columns: vec![
            col("id", SqlType::Int, false),
            col("day", SqlType::Date, true),
            col("at", SqlType::DateTime, true),
        ],
        pk: "id".into(),
        auto_increment: None,
        engine: Engine::Row,
        indexes: vec![],
    };
    let day = |s: &str| Value::Date(crate::sql::temporal::parse_date(s).unwrap());
    let at = |s: &str| Value::DateTime(crate::sql::temporal::parse_datetime(s).unwrap());
    let rows: Vec<Vec<Value>> = vec![
        vec![Value::Int(1), day("1969-12-31"), at("1969-12-31 23:59:59")],
        vec![Value::Int(2), Value::Null, Value::Null],
        vec![
            Value::Int(3),
            day("2024-02-29"),
            at("2024-02-29 13:45:59.5"),
        ],
    ];
    let (file, zones) = build_segment(&schema, &rows).unwrap();
    let (footer, _) = decode::open(&file).unwrap();
    assert_eq!(footer.num_rows, 3);
    for (ci, col) in schema.columns.iter().enumerate() {
        let expected: Vec<Value> = rows.iter().map(|r| r[ci].clone()).collect();
        assert_eq!(
            decode::decode_column(&file, &footer.columns[ci], col.sql_type).unwrap(),
            expected,
            "column {}",
            col.name
        );
        // Temporal pages are PLAIN (DICT stays string-only).
        assert!(footer.columns[ci]
            .pages
            .iter()
            .all(|p| p.encoding == ENC_PLAIN));
    }
    // Zonemaps fold by the raw i64 (days / micros), NULLs counted.
    assert_eq!(zones[1].min, day("1969-12-31"));
    assert_eq!(zones[1].max, day("2024-02-29"));
    assert_eq!(zones[1].null_count, 1);
    assert_eq!(zones[2].min, at("1969-12-31 23:59:59"));
    assert_eq!(zones[2].max, at("2024-02-29 13:45:59.5"));
    assert_eq!(zones[2].null_count, 1);
}

#[test]
fn multi_page_splits_and_ordinals() {
    let schema = int_schema();
    let n = 20_000usize;
    let rows: Vec<Vec<Value>> = (0..n).map(|i| vec![Value::Int(i as i64 * 3)]).collect();
    let (file, zones) = build_segment(&schema, &rows).unwrap();
    let (footer, _) = decode::open(&file).unwrap();
    let pages = &footer.columns[0].pages;
    assert_eq!(pages.len(), n.div_ceil(8192));
    assert_eq!(pages[0].offset, MAGIC.len() as u64);
    let mut ordinal = 0u32;
    for (i, p) in pages.iter().enumerate() {
        assert_eq!(p.first_ordinal, ordinal);
        let want = (n - ordinal as usize).min(8192) as u32;
        assert_eq!(p.num_values, want, "page {i}");
        assert_eq!(p.encoding, ENC_PLAIN);
        // Pages lie inside the file, strictly ordered and contiguous.
        assert_eq!(
            p.offset,
            MAGIC.len() as u64 + pages[..i].iter().map(|q| q.len as u64).sum::<u64>()
        );
        assert!((p.offset + p.len as u64) <= file.len() as u64);
        ordinal += p.num_values;
    }
    assert_eq!(ordinal, n as u32);
    let back = decode::decode_column(&file, &footer.columns[0], SqlType::Int).unwrap();
    assert_eq!(back.len(), n);
    assert_eq!(back[0], Value::Int(0));
    assert_eq!(back[8191], Value::Int(8191 * 3));
    assert_eq!(back[8192], Value::Int(8192 * 3));
    assert_eq!(back[n - 1], Value::Int((n - 1) as i64 * 3));
    assert_eq!(zones[0].min, Value::Int(0));
    assert_eq!(zones[0].max, Value::Int((n - 1) as i64 * 3));
}

#[test]
fn dict_encoding_wins_for_low_cardinality() {
    let schema = str_schema(true);
    let choices = ["alpha", "beta"];
    let mut rows = Vec::new();
    for i in 0..200u32 {
        let s = if i % 29 == 0 {
            Value::Null
        } else {
            Value::Str(choices[i as usize % 2].into())
        };
        rows.push(vec![Value::Int(i64::from(i)), s]);
    }
    let (file, _) = build_segment(&schema, &rows).unwrap();
    let (footer, _) = decode::open(&file).unwrap();
    let pages = &footer.columns[1].pages;
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].encoding, ENC_DICT);
    assert_eq!(pages[0].null_count, 7);
    assert_eq!(pages[0].min, Value::Str("alpha".into()));
    assert_eq!(pages[0].max, Value::Str("beta".into()));
    let back = decode::decode_column(&file, &footer.columns[1], SqlType::VarChar).unwrap();
    let expected: Vec<Value> = rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(back, expected);
}

#[test]
fn plain_encoding_fallback_for_high_cardinality() {
    let schema = str_schema(false);
    let rows: Vec<Vec<Value>> = (0..100u32)
        .map(|i| {
            vec![
                Value::Int(i64::from(i)),
                Value::Str(format!("unique-value-{i:04}")),
            ]
        })
        .collect();
    let (file, _) = build_segment(&schema, &rows).unwrap();
    let (footer, _) = decode::open(&file).unwrap();
    assert_eq!(footer.columns[1].pages[0].encoding, ENC_PLAIN);
    let back = decode::decode_column(&file, &footer.columns[1], SqlType::VarChar).unwrap();
    let expected: Vec<Value> = rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(back, expected);
}

#[test]
fn row_width_mismatch_errors() {
    let schema = str_schema(true);
    let rows = vec![vec![Value::Int(1), Value::Null, Value::Null]];
    let err = build_segment(&schema, &rows).unwrap_err();
    assert!(err.contains("width"), "{err}");
}

#[test]
fn corruption_is_detected() {
    let schema = int_schema();
    let rows: Vec<Vec<Value>> = (0..50).map(|i| vec![Value::Int(i)]).collect();
    let (file, _) = build_segment(&schema, &rows).unwrap();

    // Flip one byte of the footer JSON -> crc error.
    let mut bad = file.clone();
    bad[file.len() - 9] ^= 0xFF;
    let err = decode::open(&bad).unwrap_err();
    assert!(err.contains("crc"), "{err}");

    // Truncate the file -> error.
    let err = decode::open(&file[..file.len() - 5]).unwrap_err();
    assert!(
        err.contains("truncated") || err.contains("range") || err.contains("crc"),
        "{err}"
    );

    // Clobber the magic -> bad magic.
    let mut bad = file.clone();
    bad[0] ^= 0xFF;
    let err = decode::open(&bad).unwrap_err();
    assert!(err.contains("magic"), "{err}");

    // Tamper with footer_len -> range error (len grew beyond the file).
    let mut bad = file.clone();
    let tampered = file.len() as u32;
    bad[file.len() - 8..file.len() - 4].copy_from_slice(&tampered.to_be_bytes());
    let err = decode::open(&bad).unwrap_err();
    assert!(err.contains("range"), "{err}");

    // Corrupt a page byte -> decode fails even if the footer is intact.
    let mut bad = file.clone();
    bad[MAGIC.len()] ^= 0xFF;
    let (footer, _) = decode::open(&bad).unwrap();
    let err = decode::decode_column(&bad, &footer.columns[0], SqlType::Int).unwrap_err();
    assert!(err.contains("corrupt"), "{err}");
}

#[test]
fn engine_serde_old_and_new_catalog_json() {
    let old = r#"{"id":1,"name":"t","columns":[{"name":"id","type":"int","nullable":false}],"pk":"id","indexes":[]}"#;
    let s: TableSchema = serde_json::from_str(old).expect("old catalog json");
    assert_eq!(s.engine, Engine::Row);
    assert!(!s.engine.is_columnar());
    let new = r#"{"id":1,"name":"t","columns":[{"name":"id","type":"int","nullable":false}],"pk":"id","engine":"columnar","indexes":[]}"#;
    let s: TableSchema = serde_json::from_str(new).expect("new catalog json");
    assert_eq!(s.engine, Engine::Columnar);
    assert!(s.engine.is_columnar());
}

#[test]
fn column_zones_keep_name_type_nullable() {
    let schema = str_schema(true);
    let rows = vec![vec![Value::Int(1), Value::Str("a".into())]];
    let (_, zones): (_, Vec<ColumnZone>) = build_segment(&schema, &rows).unwrap();
    assert_eq!(zones[1].name, "s");
    assert_eq!(zones[1].sql_type, SqlType::VarChar);
    assert!(zones[1].nullable);
    assert!(!zones[0].nullable);
}
