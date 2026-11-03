//! VECTOR column-domain helpers: the `'[e,e,...]'` literal grammar,
//! the canonical text form, dimension validation and bytewise
//! equality. Pure functions only -- the value type lives in `schema`
//! (`Value::Vector`), the row payload codec in `row`, the DDL typing
//! in `parse::translate_type`.
//!
//! Vectors are an equality-only domain: two values are equal iff their
//! f32 payloads are bit-identical (NaN never round-trips a literal, so
//! the bytewise rule never surprises the SQL layer), and NO total
//! order exists -- range operators, ORDER BY and index/pk membership
//! are loud rejections, never an invented ordering.

use crate::sql::storage::schema::Value;

/// Maximum dimension of a VECTOR(N) column (DDL validates 1..=this;
/// 4*dim bytes stay well inside one row payload).
pub const MAX_VECTOR_DIM: u32 = 4096;

/// Parse a vector literal `'[1, 2, 3]'`: bracketed, comma-separated
/// f32 elements. Whitespace around the whole literal and the elements
/// is tolerated; the element grammar is Rust's f32 spelling (so
/// `1`, `1.5`, `-2e-3` all parse). Empty vectors (`[]`), empty
/// elements (trailing commas) and non-finite elements (NaN/Inf) are
/// rejected. Shared by write-path coercion and comparisons.
pub fn parse_vector_literal(s: &str) -> Result<Vec<f32>, String> {
    let t = s.trim();
    let Some(inner) = t.strip_prefix('[').and_then(|b| b.strip_suffix(']')) else {
        return Err(format!("incorrect VECTOR value: '{s}'"));
    };
    if inner.trim().is_empty() {
        return Err(format!("truncated VECTOR value: '{s}'"));
    }
    let mut out = Vec::new();
    for part in inner.split(',') {
        let e = part.trim();
        if e.is_empty() {
            return Err(format!("truncated VECTOR value: '{s}'"));
        }
        let f: f32 = e
            .parse()
            .map_err(|_| format!("incorrect VECTOR value: '{s}'"))?;
        if !f.is_finite() {
            return Err(format!("incorrect VECTOR value: '{s}'"));
        }
        out.push(f);
    }
    Ok(out)
}

/// Canonical text form `'[1,2,3]'` (Rust's shortest round-trip f32
/// Display per element): `parse(format(v))` is the identity.
pub fn format_vector(v: &[f32]) -> String {
    let mut out = String::with_capacity(2 + v.len() * 8);
    out.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&f.to_string());
    }
    out.push(']');
    out
}

/// A vector's dimension must equal the column's exactly.
pub fn validate_vector_dim(v: &[f32], expect: u32) -> Result<(), String> {
    if v.len() as u64 != u64::from(expect) {
        return Err(format!(
            "vector has {} dimensions, column expects {expect}",
            v.len()
        ));
    }
    Ok(())
}

/// Bytewise equality on the f32 payloads (the domain's only order
/// relation; see the module doc).
pub fn bits_equal(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// Coerce one write-path value into a dimension-checked vector: the
/// `'[e,e,...]'` literal spelling parses, an existing Vector value
/// re-checks the column dimension. Errors name the offender (MySQL
/// 1292 style, surfaced as WrongValue by the callers).
pub fn coerce_value(v: Value, dim: u32) -> Result<Vec<f32>, String> {
    let vec = match v {
        Value::Str(s) => parse_vector_literal(&s)?,
        Value::Vector(vals) => vals,
        other => return Err(format!("cannot store {other:?} into vector({dim}) column")),
    };
    validate_vector_dim(&vec, dim)?;
    Ok(vec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_whitespace_and_float_spellings() {
        assert_eq!(
            parse_vector_literal("[1,2,3]").unwrap(),
            vec![1.0, 2.0, 3.0]
        );
        assert_eq!(
            parse_vector_literal("  [ 1 , -2.5 , 3e-2 ]  ").unwrap(),
            vec![1.0, -2.5, 0.03]
        );
        assert_eq!(parse_vector_literal("[0.5]").unwrap(), vec![0.5]);
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "",
            "1,2,3",
            "[1,2",
            "1,2]",
            "[]",
            "[ ]",
            "[1,,2]",
            "[1,2,]",
            "[,]",
            "[abc]",
            "[1 nan]",
            "[NaN]",
            "[inf]",
            "[-Infinity]",
            "[1,0x10]",
        ] {
            assert!(parse_vector_literal(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn format_round_trips_parse() {
        for text in ["[1]", "[1,2,3]", "[1.5,-2.25,0.03]", "[0,0,0,0]"] {
            let v = parse_vector_literal(text).unwrap();
            let out = format_vector(&v);
            assert_eq!(out, text);
            assert_eq!(parse_vector_literal(&out).unwrap(), v);
        }
    }

    #[test]
    fn dim_validation_names_both_sides() {
        assert_eq!(validate_vector_dim(&[1.0, 2.0], 2), Ok(()));
        let e = validate_vector_dim(&[1.0], 2).unwrap_err();
        assert_eq!(e, "vector has 1 dimensions, column expects 2");
    }

    #[test]
    fn equality_is_bytewise() {
        assert!(!bits_equal(&[1.0, -0.0], &[1.0, 0.0]));
        assert!(!bits_equal(&[1.0], &[1.0, 1.0]));
        assert!(bits_equal(&[], &[]));
    }

    #[test]
    fn coerce_takes_literals_and_rejects_domains() {
        assert_eq!(
            coerce_value(Value::Str("[1,2]".into()), 2).unwrap(),
            vec![1.0, 2.0]
        );
        assert_eq!(
            coerce_value(Value::Vector(vec![1.0, 2.0]), 2).unwrap(),
            vec![1.0, 2.0]
        );
        assert!(coerce_value(Value::Str("[1]".into()), 2).is_err());
        assert!(coerce_value(Value::Int(3), 2).is_err());
        assert!(coerce_value(Value::Str("nope".into()), 2).is_err());
    }
}
