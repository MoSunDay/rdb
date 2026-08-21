//! Shared vector math (pure functions): the one cosine/dot/L2
//! implementation used by VSIM (exact scan) and the search engine's
//! ANN rerank so both sides always agree on distance semantics.

/// Dot product; vectors of unequal length contribute only to the
/// shorter length (callers always pass equal lengths).
pub fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

pub fn l2(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

pub fn norm(a: &[f64]) -> f64 {
    dot(a, a).sqrt()
}

/// Cosine similarity; a zero vector has no direction, so any pair
/// touching one scores 0 (-> VSIM's (cos+1)/2 = 0.5).
pub fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let (na, nb) = (norm(a), norm(b));
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot(a, b) / (na * nb)).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_reference_points() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-12);
        assert!((cosine(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-12);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-12);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn l2_reference_points() {
        assert!((l2(&[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-12);
        assert_eq!(l2(&[1.0], &[1.0]), 0.0);
    }
}
