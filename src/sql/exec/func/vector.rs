//! Vector-function family (01-vector-surface D5-D7): STRING_TO_VECTOR
//! (+ TO_VECTOR alias), VECTOR_TO_STRING (+ VEC_TO_TEXT), VECTOR_DIM
//! and DISTANCE -- the MySQL-face vector surface; the libsql-specific
//! names stay out (deviation ledger (i)).
//!
//! The distance math lives here as pure functions over f32 slices (f64
//! accumulation via `search::vecmath`, the one implementation the
//! search side already uses) so the exact-KNN collector in
//! `exec::vector_knn` reuses it and can never disagree with the scalar
//! function.

use crate::search::vecmath;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::Value;
use crate::sql::storage::vector as vecstore;

use super::wrong_param_count;

/// The DISTANCE metrics (D6): case-insensitive names, exactly three
/// of them -- no 'l2' alias, the plan pins the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Metric {
    Cosine,
    Euclidean,
    Dot,
}

/// Parse one metric name (ASCII case-insensitive). Errors carry the
/// D6 wording (MySQL's own text is still unmeasured, ledger).
pub(crate) fn parse_metric(s: &str) -> SqlResult<Metric> {
    match s.to_ascii_lowercase().as_str() {
        "cosine" => Ok(Metric::Cosine),
        "euclidean" => Ok(Metric::Euclidean),
        "dot" => Ok(Metric::Dot),
        _ => Err(SqlError::new(
            ErrorCode::WrongValue,
            format!(
                "Invalid distance metric '{s}' for function DISTANCE. \
                 Must be one of: COSINE, EUCLIDEAN, DOT"
            ),
        )),
    }
}

/// Distance between two equal-width vectors, accumulated in f64 (D7):
/// cosine = 1 - similarity (a zero vector has no direction, so its
/// cosine distance is 1 -- vecmath semantics, not libsql's NaN),
/// euclidean = L2, dot = NEGATED inner product so every metric reads
/// "smaller = closer". Dimension mismatch is a loud D10 error.
pub(crate) fn distance(a: &[f32], b: &[f32], m: Metric) -> SqlResult<f64> {
    if a.len() != b.len() {
        return Err(SqlError::new(
            ErrorCode::WrongValue,
            format!(
                "Vector dimension mismatch: expected {}, got {}",
                a.len(),
                b.len()
            ),
        ));
    }
    let widen = |v: &[f32]| -> Vec<f64> { v.iter().map(|f| f64::from(*f)).collect() };
    let (a, b) = (widen(a), widen(b));
    Ok(match m {
        Metric::Cosine => 1.0 - vecmath::cosine(&a, &b),
        Metric::Euclidean => vecmath::l2(&a, &b),
        Metric::Dot => -vecmath::dot(&a, &b),
    })
}

/// Coerce one vector-domain operand: a Vector passes through, text
/// parses as the '[e,e,...]' literal (D9), anything else is a loud
/// wrong-value error. NULL is the caller's business (three-valued
/// logic propagates before coercion). No width check here -- the
/// function face has no declared column dimension.
pub(crate) fn vector_arg(name: &str, v: &Value) -> SqlResult<Vec<f32>> {
    match v {
        Value::Vector(vals) => Ok(vals.clone()),
        Value::Str(s) => vecstore::parse_vector_literal(s).map_err(|_| wrong_vector_arg(name, v)),
        _ => Err(wrong_vector_arg(name, v)),
    }
}

/// Wrong-domain operand error (MySQL 1292 flavor).
fn wrong_vector_arg(name: &str, v: &Value) -> SqlError {
    SqlError::new(
        ErrorCode::WrongValue,
        format!("Incorrect VECTOR value for function {name}: {v:?}"),
    )
}

/// Evaluate one vector-family function; `None` = name not owned here.
pub fn eval(name: &str, args: &[Value]) -> Option<SqlResult<Value>> {
    match name {
        "string_to_vector" | "to_vector" => Some(string_to_vector(name, args)),
        "vector_to_string" | "vec_to_text" => Some(vector_to_string(name, args)),
        "vector_dim" => Some(vector_dim(name, args)),
        "distance" => Some(eval_distance(args)),
        _ => None,
    }
}

/// STRING_TO_VECTOR('[..]') / TO_VECTOR: text -> Vector. A Vector
/// operand passes through (the write-path coercion domain); NULL is
/// NULL.
fn string_to_vector(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    Ok(Value::Vector(vector_arg(name, v)?))
}

/// VECTOR_TO_STRING / VEC_TO_TEXT: Vector (or the literal spelling)
/// -> canonical '[e,e,...]' text.
fn vector_to_string(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    Ok(Value::Str(vecstore::format_vector(&vector_arg(name, v)?)))
}

/// VECTOR_DIM: element count of the vector (or of the literal).
fn vector_dim(name: &str, args: &[Value]) -> SqlResult<Value> {
    let [v] = args else {
        return Err(wrong_param_count(name));
    };
    if matches!(v, Value::Null) {
        return Ok(Value::Null);
    }
    Ok(Value::Int(vector_arg(name, v)?.len() as i64))
}

/// Metric operand of DISTANCE: a text name parses (ASCII
/// case-insensitive), NULL is NULL, anything else is a loud
/// wrong-value error. Shared by the scalar call and the KNN fast
/// path, like everything else in this module.
pub(crate) fn metric_arg(v: &Value) -> SqlResult<Option<Metric>> {
    match v {
        Value::Null => Ok(None),
        Value::Str(s) => Ok(Some(parse_metric(s)?)),
        other => Err(wrong_vector_arg("distance", other)),
    }
}

/// Value-level DISTANCE(v1, v2, metric): the entry both the scalar
/// call and the KNN fast path go through, so the two cannot drift.
/// Any NULL operand is NULL; the metric must name a known distance.
pub(crate) fn eval_distance(args: &[Value]) -> SqlResult<Value> {
    let wrong = || wrong_param_count("distance");
    let [a, b, m] = args else {
        return Err(wrong());
    };
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    let Some(metric) = metric_arg(m)? else {
        return Ok(Value::Null);
    };
    let (va, vb) = (vector_arg("distance", a)?, vector_arg("distance", b)?);
    Ok(Value::Double(distance(&va, &vb, metric)?))
}

#[cfg(test)]
#[path = "vector_tests.rs"]
mod tests;
