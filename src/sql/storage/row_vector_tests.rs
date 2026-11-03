//! VECTOR column storage tests: row payload codec (schema-width f32
//! array), the self-describing typed tag 0x09, key-encoding refusal
//! (equality-only domain), catalog JSON round trip and bytewise
//! `PartialEq`.

use super::*;
use crate::sql::storage::codec::{decode_key, decode_typed, encode_key, encode_typed};
use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel};

fn schema(dim: u32) -> TableSchema {
    TableSchema {
        id: 11,
        name: "vec_t".into(),
        columns: vec![
            ColumnDef {
                name: "id".into(),
                sql_type: SqlType::Int,
                nullable: false,
            },
            ColumnDef {
                name: "v".into(),
                sql_type: SqlType::Vector { dim },
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
fn row_round_trip_with_vector_and_null() {
    let s = schema(3);
    let rows = [
        vec![Value::Int(1), Value::Vector(vec![1.0, -2.5, 0.03])],
        vec![Value::Int(2), Value::Null],
        vec![Value::Int(3), Value::Vector(vec![0.0, 5.0, -0.0])],
    ];
    for row in &rows {
        let bytes = encode_row(&s, row).expect("encode");
        assert_eq!(
            decode_version(&s, &bytes).expect("decode").1,
            *row,
            "round trip"
        );
    }
    // Payload width is exactly 4*dim little-endian f32s: 1B int + 12B.
    let bytes = encode_row(&s, &rows[0]).unwrap();
    assert_eq!(bytes.len(), 1 + 1 + 8 + 12);
    let want: Vec<u8> = [1.0f32, -2.5, 0.03]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    assert_eq!(&bytes[bytes.len() - 12..], &want[..]);
}

#[test]
fn row_codec_rejects_drift_and_truncation() {
    let s = schema(3);
    // Encode: value dimension must equal the schema's.
    let bad = vec![Value::Int(1), Value::Vector(vec![1.0, 2.0])];
    let e = encode_row(&s, &bad).unwrap_err();
    assert!(e.contains("vector dimension mismatch"), "{e}");
    // Decode: a short payload is truncated, never misread.
    let ok = vec![Value::Int(1), Value::Vector(vec![1.0, 2.0, 3.0])];
    let full = encode_row(&s, &ok).unwrap();
    let cut = &full[..full.len() - 4];
    let e = decode_version(&s, cut).unwrap_err();
    assert!(e.contains("vector payload truncated"), "{e}");
}

#[test]
fn typed_tag_nine_round_trip_and_malformed() {
    let v = Value::Vector(vec![1.5, -2.0]);
    let b = encode_typed(&v);
    assert_eq!(b[0], 0x09, "vector typed tag");
    assert_eq!(b.len(), 1 + 8, "4B per element, no length prefix");
    assert_eq!(decode_typed(&b).unwrap(), (v.clone(), &[][..]));
    // Chained: an int payload ahead of the vector splits cleanly (the
    // vector consumes the REST of its input).
    let chained = [encode_typed(&Value::Int(7)), b].concat();
    let (head, rest) = decode_typed(&chained).unwrap();
    assert_eq!(head, Value::Int(7));
    assert_eq!(decode_typed(rest).unwrap().0, v);
    // Malformed: empty payload and non-multiple-of-4 lengths.
    assert!(decode_typed(&[0x09]).is_err());
    assert!(decode_typed(&[0x09, 1, 2, 3, 4, 5]).is_err());
}

#[test]
fn vectors_are_never_key_components() {
    let e = encode_key(&Value::Vector(vec![1.0])).unwrap_err();
    assert!(e.contains("cannot be key components"), "{e}");
    let ty = SqlType::Vector { dim: 1 };
    assert!(decode_key(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0], ty).is_err());
}

#[test]
fn catalog_json_round_trips_vector_type_and_value() {
    let mut s = schema(4);
    s.columns[1].sql_type = SqlType::Vector { dim: 4096 };
    let js = serde_json::to_string(&s).expect("ser");
    assert!(js.contains(r#""type":{"vector":{"dim":4096}}"#), "{js}");
    assert_eq!(serde_json::from_str::<TableSchema>(&js).unwrap(), s);
    let v = serde_json::to_value(Value::Vector(vec![0.5, 1.5])).unwrap();
    assert_eq!(
        serde_json::from_value::<Value>(v).unwrap(),
        Value::Vector(vec![0.5, 1.5])
    );
}

#[test]
fn value_equality_is_bytewise_for_vectors() {
    assert_eq!(Value::Vector(vec![1.0]), Value::Vector(vec![1.0]));
    // 0.0 and -0.0 are == as f32 but differ in bits: not equal here.
    assert_ne!(Value::Vector(vec![0.0]), Value::Vector(vec![-0.0]));
    assert_ne!(Value::Vector(vec![1.0]), Value::Vector(vec![1.0, 1.0]));
    // Other variants keep derived semantics.
    assert_eq!(Value::Double(0.0), Value::Double(-0.0));
}
