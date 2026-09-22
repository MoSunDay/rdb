//! Unit tests for `es::eval`'s pure pieces: the knn nprobe
//! mapping and the candidate-set combination helpers (Store-bound
//! behavior is covered by e2e tests, like `src/search`).

use super::*;

#[test]
fn nprobe_mapping() {
    assert_eq!(nprobe_of(1), 1); // clamped up
    assert_eq!(nprobe_of(16), 1);
    assert_eq!(nprobe_of(100), 6); // ES-ish default candidates
    assert_eq!(nprobe_of(10_000), 64); // clamped down
}

#[test]
fn set_ops_union_and_intersect() {
    let mut a: HashMap<Vec<u8>, f64> = HashMap::new();
    a.insert(b"d1".to_vec(), 1.0);
    a.insert(b"d2".to_vec(), 2.0);
    let mut b: HashMap<Vec<u8>, f64> = HashMap::new();
    b.insert(b"d2".to_vec(), 3.0);
    let mut union = a.clone();
    union_sum(&b, &mut union);
    assert_eq!(union.len(), 2);
    assert_eq!(union[&b"d2".to_vec()], 5.0); // scores sum
    retain_common(&mut a, &b);
    assert_eq!(a.len(), 1);
    assert!(a.contains_key(b"d2".as_slice()));
}
