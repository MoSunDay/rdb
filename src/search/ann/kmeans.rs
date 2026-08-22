//! Seeded k-means for centroid training: deterministic LCG (no rand
//! dependency -- replicas must converge on identical tables),
//! k-means++ spread seeding and Lloyd iterations over f64 vectors.
//! Centroids are returned at the stored f32 precision.

use crate::search::vecmath;

/// Deterministic LCG step (no rand dependency; k-means must be
/// reproducible across replica nodes).
fn next_u64(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state >> 33
}

/// Lloyd's k-means over f64 vectors; centroids returned as f32 (the
/// stored precision). Init picks seeded distinct vectors; empty
/// clusters keep their previous centroid. `k` is clamped to [#vectors].
pub fn train(vectors: &[Vec<f64>], dim: usize, k: usize, iters: usize, seed: u64) -> Vec<Vec<f32>> {
    let k = k.max(1).min(vectors.len().max(1));
    let mut state = seed | 1;
    let mut centroids: Vec<Vec<f64>> = Vec::with_capacity(k);
    let mut chosen = vec![false; vectors.len()];
    let mut chosen_count = 0usize;
    while centroids.len() < k && chosen_count < vectors.len() {
        let idx = pick_seed(&mut state, &centroids, vectors, &chosen);
        let Some(idx) = idx else { break };
        chosen[idx] = true;
        chosen_count += 1;
        centroids.push(vectors[idx].clone());
    }
    for _ in 0..iters.max(1) {
        let mut sums = vec![vec![0.0f64; dim]; centroids.len()];
        let mut counts = vec![0usize; centroids.len()];
        for v in vectors {
            let (best, _) = nearest(&centroids, v);
            counts[best] += 1;
            for (axis, &x) in v.iter().enumerate().take(dim) {
                sums[best][axis] += x;
            }
        }
        for (c, (sum, &n)) in centroids.iter_mut().zip(sums.iter().zip(&counts)) {
            if n > 0 {
                for axis in 0..dim {
                    c[axis] = sum[axis] / n as f64;
                }
            }
        }
    }
    centroids
        .iter()
        .map(|c| c.iter().map(|&x| x as f32).collect())
        .collect()
}

/// Next k-means seed. The first is uniform; later ones follow k-means++
/// -- unchosen points weighted by squared distance to their nearest
/// chosen seed, so seeds spread across clusters instead of collapsing
/// onto one (plain Lloyd never splits a duplicated seed). Deterministic
/// under the caller's LCG state.
fn pick_seed(
    state: &mut u64,
    centroids: &[Vec<f64>],
    vectors: &[Vec<f64>],
    chosen: &[bool],
) -> Option<usize> {
    if vectors.is_empty() {
        return None;
    }
    if centroids.is_empty() {
        return Some(next_u64(state) as usize % vectors.len());
    }
    let mut weights = vec![0u64; vectors.len()];
    let mut total = 0u64;
    for (i, v) in vectors.iter().enumerate() {
        if chosen[i] {
            continue;
        }
        let (_, d) = nearest(centroids, v);
        let clamped = d.min(1e6);
        let w = (clamped * clamped).max(1e-12) as u64;
        weights[i] = w.max(1);
        total = total.saturating_add(weights[i]);
    }
    if total == 0 {
        return (0..vectors.len()).find(|&i| !chosen[i]);
    }
    let mut r = next_u64(state) % total;
    for (i, &w) in weights.iter().enumerate() {
        if r < w {
            return Some(i);
        }
        r -= w;
    }
    (0..vectors.len()).find(|&i| !chosen[i])
}

/// (index, distance) of the nearest centroid by L2.
pub fn nearest(centroids: &[Vec<f64>], v: &[f64]) -> (usize, f64) {
    let mut best = (0usize, f64::INFINITY);
    for (i, c) in centroids.iter().enumerate() {
        let d = vecmath::l2(c, v);
        if d < best.1 {
            best = (i, d);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kmeans_recovers_two_clusters() {
        let vectors: Vec<Vec<f64>> = (0..20)
            .map(|i| {
                let near_zero = i % 2 == 0;
                vec![
                    if near_zero { 0.1 } else { 10.0 },
                    if near_zero { 0.2 } else { 9.8 },
                ]
            })
            .collect();
        let centroids = train(&vectors, 2, 2, 5, 7);
        assert_eq!(centroids.len(), 2);
        let f64s: Vec<Vec<f64>> = centroids
            .iter()
            .map(|c| c.iter().map(|&x| f64::from(x)).collect())
            .collect();
        let (d0, d1) = (
            vecmath::l2(&f64s[0], &[0.0, 0.0]),
            vecmath::l2(&f64s[1], &[0.0, 0.0]),
        );
        // one centroid lands on each cluster
        assert!(d0.min(d1) < 1.0, "d0={d0} d1={d1}");
        assert!(d0.max(d1) > 9.0, "d0={d0} d1={d1}");
        // deterministic across runs (same seed, same data)
        assert_eq!(centroids, train(&vectors, 2, 2, 5, 7));
    }
}
