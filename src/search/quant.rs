//! SQ8 scalar quantization: one f32/f64 vector -> one u8 per dimension
//! plus a per-dimension calibration (min, scale) stored ONCE per index
//! field in the centroid record (`index_codec::CentroidTable`), so a
//! quantized posting entry costs ~1 byte per dimension.
//!
//! dequant(d) = min_d + q_d * scale_d, scale = (max-min)/255; encode
//! CLAMPS out-of-calibration values to [0,255] (standard SQ behavior:
//! vectors added after the calibration drift slightly, never corrupt).
//! Per-dim error <= scale/2, which bounds SQ8 top-k recall vs exact --
//! the rerank step in `ann` restores exact ordering for finalists.

use super::vecmath;

/// Per-dimension calibration over a sample of vectors: min and
/// (max-min)/255 per axis. Degenerate axes (max == min) get scale
/// 1/255 * eps floor to avoid div-by-zero (any q decodes to min).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Calibration {
    pub min: Vec<f32>,
    pub scale: Vec<f32>,
}

/// Fit calibration over `vectors` (all must share one dimensionality,
/// callers guarantee it; mismatched vectors are skipped).
pub fn fit(vectors: &[Vec<f64>], dim: usize) -> Calibration {
    let mut min = vec![f32::INFINITY; dim];
    let mut max = vec![f32::NEG_INFINITY; dim];
    for v in vectors {
        if v.len() != dim {
            continue;
        }
        for (axis, &x) in v.iter().enumerate() {
            let x = x as f32;
            if x < min[axis] {
                min[axis] = x;
            }
            if x > max[axis] {
                max[axis] = x;
            }
        }
    }
    let scale = min
        .iter()
        .zip(&max)
        .map(|(&lo, &hi)| ((hi - lo) / 255.0).max(1e-9))
        .collect();
    Calibration { min, scale }
}

/// Quantize with the field-global calibration; clamps to [0,255].
pub fn encode(cal: &Calibration, v: &[f64]) -> Vec<u8> {
    v.iter()
        .enumerate()
        .map(|(axis, &x)| {
            let q = ((x as f32 - cal.min[axis]) / cal.scale[axis]).round();
            q.clamp(0.0, 255.0) as u8
        })
        .collect()
}

/// Dequantize back to f64 (for distance computation).
pub fn decode(cal: &Calibration, q: &[u8]) -> Vec<f64> {
    q.iter()
        .enumerate()
        .map(|(axis, &b)| cal.min[axis] as f64 + f64::from(b) * cal.scale[axis] as f64)
        .collect()
}

/// L2 distance between the exact query and a quantized entry,
/// dequantizing lazily per axis (no intermediate Vec in hot loops --
/// same formula as `vecmath::l2` on the decoded vector).
pub fn l2_dequant(cal: &Calibration, q: &[u8], query: &[f64]) -> f64 {
    let mut sum = 0.0;
    for (axis, (&b, &x)) in q.iter().zip(query).enumerate() {
        let d = (cal.min[axis] as f64 + f64::from(b) * cal.scale[axis] as f64) - x;
        sum += d * d;
    }
    sum.sqrt()
}

/// Relative reconstruction error of a roundtrip (test/diagnostic aid).
pub fn roundtrip_error(cal: &Calibration, v: &[f64]) -> f64 {
    let back = decode(cal, &encode(cal, v));
    let denom = vecmath::norm(v).max(1e-9);
    vecmath::l2(v, &back) / denom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_error_bounded_by_half_scale_per_axis() {
        let dim = 8;
        let vectors: Vec<Vec<f64>> = (0..64)
            .map(|i| {
                (0..dim)
                    .map(|d| (i * 7 + d * 3) as f64 % 10.0 - 5.0)
                    .collect()
            })
            .collect();
        let cal = fit(&vectors, dim);
        for v in &vectors {
            assert!(roundtrip_error(&cal, v) < 0.05, "v={v:?}");
        }
    }

    #[test]
    fn encode_clamps_out_of_calibration() {
        let cal = fit(&[vec![0.0, 0.0], vec![10.0, 10.0]], 2);
        let q = encode(&cal, &[-100.0, 100.0]);
        assert_eq!(q, vec![0, 255]);
        let mid = decode(&cal, &encode(&cal, &[5.0, 5.0]));
        assert!((mid[0] - 5.0).abs() < 5.0 / 255.0 + 1e-9);
    }

    #[test]
    fn l2_dequant_matches_decode_then_l2() {
        let dim = 4;
        let vectors: Vec<Vec<f64>> = (0..16)
            .map(|i| (0..dim).map(|d| ((i + d * 5) % 11) as f64).collect())
            .collect();
        let cal = fit(&vectors, dim);
        let q = encode(&cal, &vectors[3]);
        let direct = l2_dequant(&cal, &q, &vectors[7]);
        let via_decode = vecmath::l2(&decode(&cal, &q), &vectors[7]);
        assert!((direct - via_decode).abs() < 1e-12);
    }

    #[test]
    fn degenerate_axis_does_not_nan() {
        let cal = fit(&[vec![1.0, 2.0], vec![1.0, 2.0]], 2);
        let q = encode(&cal, &[1.0, 2.0]);
        assert!(decode(&cal, &q).iter().all(|x| x.is_finite()));
    }
}
