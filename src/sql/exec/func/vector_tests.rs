//! Vector-family evaluation: coercion, aliases, the three metrics,
//! zero-vector/NULL semantics and the loud D6/D10 error wordings.

use super::*;

fn f(name: &str, args: &[Value]) -> Value {
    eval(name, args)
        .expect("family owns the name")
        .expect("evaluates")
}

fn fe(name: &str, args: &[Value]) -> crate::sql::parse::error::SqlError {
    eval(name, args).expect("family owns the name").unwrap_err()
}

fn v(elems: &[f32]) -> Value {
    Value::Vector(elems.to_vec())
}

#[test]
fn family_owns_only_its_names() {
    assert!(eval("vector_distance_cos", &[v(&[1.0])]).is_none());
    assert!(eval("libsql_vector_idx", &[]).is_none());
}

#[test]
fn string_to_vector_parses_and_aliases() {
    assert_eq!(
        f("string_to_vector", &[Value::Str("[1, -2.5, 3e-2]".into())]),
        v(&[1.0, -2.5, 0.03])
    );
    assert_eq!(f("to_vector", &[Value::Str("[0.5]".into())]), v(&[0.5]));
    // Vector passthrough, NULL propagation, arity, wrong domains.
    assert_eq!(f("string_to_vector", &[v(&[1.0])]), v(&[1.0]));
    assert_eq!(f("to_vector", &[Value::Null]), Value::Null);
    let e = fe("string_to_vector", &[]);
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
    let e = fe("string_to_vector", &[Value::Int(3)]);
    assert!(e.msg.contains("Incorrect VECTOR value"), "{e}");
    let e = fe("string_to_vector", &[Value::Str("[1,".into())]);
    assert!(e.msg.contains("Incorrect VECTOR value"), "{e}");
}

#[test]
fn vector_to_string_canonicalizes_and_aliases() {
    assert_eq!(
        f("vector_to_string", &[v(&[1.0, -2.5, 0.03])]),
        Value::Str("[1,-2.5,0.03]".into())
    );
    assert_eq!(
        f("vec_to_text", &[Value::Str("  [ 1 , 2 ] ".into())]),
        Value::Str("[1,2]".into())
    );
    assert_eq!(f("vector_to_string", &[Value::Null]), Value::Null);
}

#[test]
fn vector_dim_counts_elements() {
    assert_eq!(f("vector_dim", &[v(&[1.0, 2.0, 3.0])]), Value::Int(3));
    assert_eq!(f("vector_dim", &[Value::Str("[1]".into())]), Value::Int(1));
    assert_eq!(f("vector_dim", &[Value::Null]), Value::Null);
}

#[test]
fn distance_metrics_reference_points() {
    let a = v(&[2.0, 0.0]);
    let m = |s: &str| Value::Str(s.into());
    // Orthogonal: cos 0 -> distance 1; identical: distance 0;
    // opposite: cos -1 -> distance 2.
    assert_eq!(
        f("distance", &[a.clone(), v(&[0.0, 3.0]), m("cosine")]),
        Value::Double(1.0)
    );
    assert_eq!(
        f("distance", &[a.clone(), a.clone(), m("COSINE")]),
        Value::Double(0.0)
    );
    assert_eq!(
        f("distance", &[a.clone(), v(&[-2.0, 0.0]), m("Cosine")]),
        Value::Double(2.0)
    );
    // Zero vector has no direction: similarity 0 -> distance 1 (D7).
    assert_eq!(
        f("distance", &[v(&[0.0, 0.0]), a.clone(), m("cosine")]),
        Value::Double(1.0)
    );
    // dot = NEGATED inner product (smaller = closer); 3-4-5 for L2.
    assert_eq!(
        f("distance", &[a.clone(), v(&[1.0, 1.0]), m("dot")]),
        Value::Double(-2.0)
    );
    assert_eq!(
        f(
            "distance",
            &[v(&[0.0, 0.0]), v(&[3.0, 4.0]), m("euclidean")]
        ),
        Value::Double(5.0)
    );
    // f32 inputs, f64 accumulation.
    let big = v(&[3e38_f32, 3e38_f32]);
    let Value::Double(d) = f("distance", &[big.clone(), big, m("dot")]) else {
        panic!("double")
    };
    assert!(
        (d + 2.0 * 3e38_f32 as f64 * 3e38_f32 as f64).abs() < 1e64,
        "{d}"
    );
}

#[test]
fn distance_null_and_arity() {
    let n = Value::Null;
    let m = Value::Str("dot".into());
    assert_eq!(
        f("distance", &[n.clone(), v(&[1.0]), m.clone()]),
        Value::Null
    );
    assert_eq!(
        f("distance", &[v(&[1.0]), n.clone(), m.clone()]),
        Value::Null
    );
    assert_eq!(f("distance", &[v(&[1.0]), v(&[1.0]), n]), Value::Null);
    let e = fe("distance", &[v(&[1.0])]);
    assert!(e.msg.contains("Incorrect parameter count"), "{e}");
}

#[test]
fn distance_rejects_bad_metric_and_dim_mismatch() {
    let m = Value::Str("l2".into()); // not in D6's enum: no alias
    let e = fe("distance", &[v(&[1.0]), v(&[1.0]), m]);
    assert_eq!(
        e.msg,
        "Invalid distance metric 'l2' for function DISTANCE. Must be one of: COSINE, EUCLIDEAN, DOT"
    );
    let e = fe(
        "distance",
        &[v(&[1.0, 2.0, 3.0]), v(&[1.0]), Value::Str("cosine".into())],
    );
    assert_eq!(e.msg, "Vector dimension mismatch: expected 3, got 1");
    let e = fe(
        "distance",
        &[Value::Int(1), v(&[1.0]), Value::Str("dot".into())],
    );
    assert!(e.msg.contains("Incorrect VECTOR value"), "{e}");
}
