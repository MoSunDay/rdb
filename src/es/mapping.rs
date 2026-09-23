//! ES index mappings (JSON) <-> kernel schema: `fields_of_mappings`
//! turns a `PUT /{index}` body into [`IndexField`]s (the same schema
//! FT.CREATE carries), `mappings_json` is the reverse for
//! `GET /{index}`. Pure byte/JSON shaping, no Store access.
//!
//! COMPAT decisions: every ES numeric type folds to Numeric (and
//! reads back as `long`); extra field options (analyzer,
//! ignore_above, similarity, ...) are accepted and ignored; a body
//! without `mappings` is a valid empty schema, like ES.

use serde_json::{json, Map, Value};

use crate::search::index_codec::{FieldType, IndexField};

/// All ES numeric types this frontend accepts; every one maps to the
/// kernel's single f64 Numeric field.
const NUMERIC_TYPES: [&str; 9] = [
    "long",
    "integer",
    "short",
    "byte",
    "double",
    "float",
    "half_float",
    "scaled_float",
    "unsigned_long",
];

/// Kernel dense-vector dim bounds (matches FT.CREATE VECTOR DIM).
const DIM_MIN: u64 = 1;
const DIM_MAX: u64 = 4096;

/// Schema of a `PUT /{index}` body: `{"mappings":{"properties":{..}}}`
/// (mappings/properties absent -> empty schema). Duplicate field
/// names, unknown types and bad dense_vector dims are errors; the
/// error strings are ES `mapper_parsing_exception` reasons.
pub fn fields_of_mappings(body: &[u8]) -> Result<Vec<IndexField>, String> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Vec::new());
    }
    let root: Value =
        serde_json::from_slice(body).map_err(|e| format!("failed to parse mappings: {e}"))?;
    let Some(props) = root.get("mappings").and_then(|m| m.get("properties")) else {
        return Ok(Vec::new());
    };
    let Some(map) = props.as_object() else {
        return Err("[mappings.properties] must be an object".to_string());
    };
    let mut fields = Vec::with_capacity(map.len());
    for (name, def) in map {
        push_field(&mut fields, field_of(name, def)?)?;
    }
    Ok(fields)
}

/// Append `field`, rejecting duplicate names. Unreachable via JSON
/// (serde collapses duplicate object keys) but keeps the invariant
/// for programmatic callers.
fn push_field(fields: &mut Vec<IndexField>, field: IndexField) -> Result<(), String> {
    if fields.iter().any(|f: &IndexField| f.name == field.name) {
        return Err(format!(
            "duplicate field name '{}'",
            String::from_utf8_lossy(&field.name)
        ));
    }
    fields.push(field);
    Ok(())
}

/// One property definition -> one [`IndexField`]; only `type` (and
/// `dims` for dense_vector) is inspected.
fn field_of(name: &str, def: &Value) -> Result<IndexField, String> {
    let Some(obj) = def.as_object() else {
        return Err(format!("field [{name}] definition must be an object"));
    };
    let Some(t) = obj.get("type").and_then(Value::as_str) else {
        return Err(format!("field [{name}] is missing the 'type' property"));
    };
    let ftype = match t {
        "text" => FieldType::Text,
        "keyword" => FieldType::Keyword,
        "dense_vector" => {
            let Some(dims) = obj.get("dims") else {
                return Err(format!(
                    "the [dims] property must be specified for field [{name}]"
                ));
            };
            let Some(dims) = dims.as_u64() else {
                return Err(format!(
                    "the [dims] property of field [{name}] must be an integer"
                ));
            };
            if !(DIM_MIN..=DIM_MAX).contains(&dims) {
                return Err(format!(
                    "the [dims] property of field [{name}] must be between {DIM_MIN} and {DIM_MAX}"
                ));
            }
            FieldType::Vector { dim: dims }
        }
        t if NUMERIC_TYPES.contains(&t) => FieldType::Numeric,
        t => return Err(format!("unknown field type '{t}'")),
    };
    Ok(IndexField {
        name: name.as_bytes().to_vec(),
        ftype,
    })
}

/// Reverse of [`fields_of_mappings`]: the `mappings` object for
/// `GET /{index}` (Numeric reads back as `long`, vectors carry their
/// `dims`).
pub fn mappings_json(fields: &[IndexField]) -> Value {
    let mut props = Map::new();
    for f in fields {
        let name = String::from_utf8_lossy(&f.name).into_owned();
        let def = match &f.ftype {
            FieldType::Text => json!({"type": "text"}),
            FieldType::Keyword => json!({"type": "keyword"}),
            FieldType::Numeric => json!({"type": "long"}),
            FieldType::Vector { dim } => json!({"type": "dense_vector", "dims": dim}),
        };
        props.insert(name, def);
    }
    json!({"properties": Value::Object(props)})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(body: &[u8]) -> Vec<IndexField> {
        fields_of_mappings(body).unwrap()
    }

    #[test]
    fn empty_and_absent_mappings_are_empty_schemas() {
        for body in [
            &b""[..],
            b"   ",
            b"{}",
            br#"{"aliases":{}}"#,
            br#"{"mappings":{}}"#,
        ] {
            assert!(fields(body).is_empty(), "{body:?}");
        }
    }

    #[test]
    fn roundtrip_all_field_types() {
        let body = br#"{"mappings":{"properties":{
            "title":{"type":"text","analyzer":"standard","ignore_above":9},
            "tag":{"type":"keyword","ignore_above":100},
            "n":{"type":"integer"},
            "v":{"type":"dense_vector","dims":8,"index":true}
        }}}"#;
        let f = fields(body);
        assert_eq!(
            f,
            vec![
                IndexField {
                    name: b"title".to_vec(),
                    ftype: FieldType::Text
                },
                IndexField {
                    name: b"tag".to_vec(),
                    ftype: FieldType::Keyword
                },
                IndexField {
                    name: b"n".to_vec(),
                    ftype: FieldType::Numeric
                },
                IndexField {
                    name: b"v".to_vec(),
                    ftype: FieldType::Vector { dim: 8 }
                },
            ]
        );
        // every numeric type folds to Numeric
        for t in ["double", "half_float", "scaled_float", "unsigned_long"] {
            let body = format!(r#"{{"mappings":{{"properties":{{"x":{{"type":"{t}"}}}}}}}}"#);
            assert_eq!(fields(body.as_bytes())[0].ftype, FieldType::Numeric);
        }
        // reverse roundtrip (numerics read back as long)
        let j = mappings_json(&f);
        assert_eq!(j["properties"]["title"], json!({"type": "text"}));
        assert_eq!(j["properties"]["n"], json!({"type": "long"}));
        assert_eq!(
            j["properties"]["v"],
            json!({"type": "dense_vector", "dims": 8})
        );
        let back = fields_of_mappings(json!({"mappings": j}).to_string().as_bytes()).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn dense_vector_dims_validation() {
        let bad: [&[u8]; 4] = [
            br#"{"mappings":{"properties":{"v":{"type":"dense_vector"}}}}"#,
            br#"{"mappings":{"properties":{"v":{"type":"dense_vector","dims":"x"}}}}"#,
            br#"{"mappings":{"properties":{"v":{"type":"dense_vector","dims":0}}}}"#,
            br#"{"mappings":{"properties":{"v":{"type":"dense_vector","dims":4097}}}}"#,
        ];
        for body in bad {
            let err = fields_of_mappings(body).unwrap_err();
            assert!(err.contains("dims"), "{body:?}: {err}");
        }
        let ok = br#"{"mappings":{"properties":{"v":{"type":"dense_vector","dims":4096}}}}"#;
        assert_eq!(fields(ok)[0].ftype, FieldType::Vector { dim: 4096 });
    }

    #[test]
    fn unknown_type_and_duplicates() {
        let err = fields_of_mappings(br#"{"mappings":{"properties":{"x":{"type":"geo_point"}}}}"#)
            .unwrap_err();
        assert_eq!(err, "unknown field type 'geo_point'");
        // serde_json collapses exact-duplicate JSON keys (last wins),
        // so the duplicate-name guard in `fields_of_mappings` is
        // defense-in-depth, never a JSON-visible error.
        let f =
            fields(br#"{"mappings":{"properties":{"x":{"type":"text"},"x":{"type":"keyword"}}}}"#);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].ftype, FieldType::Keyword);
        // the guard itself:
        let mut dup = vec![IndexField {
            name: b"x".to_vec(),
            ftype: FieldType::Text,
        }];
        let err = push_field(
            &mut dup,
            IndexField {
                name: b"x".to_vec(),
                ftype: FieldType::Keyword,
            },
        );
        assert_eq!(err.unwrap_err(), "duplicate field name 'x'");
    }
}
