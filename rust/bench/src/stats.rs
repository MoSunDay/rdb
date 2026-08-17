//! Aggregation and reporting: pure functions over collected samples.

use crate::cli::Config;
use crate::client::ClientStats;

/// Mean of `xs`; 0.0 for an empty slice.
fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Nearest-rank percentile over an already sorted slice: the
/// ceil(p/100 * n)-th smallest value (p50 of [1..=100] is the 50th).
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Merge per-client stats, print the report block (one line each, meant to
/// be parsed by scripts) and return the process exit code: 1 when any
/// server reply was an error, 0 otherwise.
pub fn report(cfg: &Config, wall_secs: f64, all: &[ClientStats]) -> i32 {
    let ops: u64 = all.iter().map(|s| s.ops).sum();
    let errors: u64 = all.iter().map(|s| s.errors).sum();
    let first_error = all.iter().find_map(|s| s.first_error.clone());

    let mut samples: Vec<f64> = all.iter().flat_map(|s| s.samples.iter().copied()).collect();
    samples.sort_by(f64::total_cmp);

    println!(
        "workload={} addr={} clients={} pipeline={} duration={}s",
        cfg.workload.as_str(),
        cfg.addr,
        cfg.clients,
        cfg.pipeline,
        cfg.duration
    );
    println!("ops={} errors={} wall={:.3}", ops, errors, wall_secs);
    println!("ops/s={:.1}", ops as f64 / wall_secs.max(1e-9));
    println!(
        "rtt_ms avg={:.3} p50={:.3} p99={:.3} max={:.3}",
        mean(&samples),
        percentile(&samples, 50.0),
        percentile(&samples, 99.0),
        samples.last().copied().unwrap_or(0.0)
    );
    if let Some(text) = first_error {
        eprintln!("first_error={text}");
    }
    i32::from(errors > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank() {
        let xs: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        assert_eq!(percentile(&xs, 50.0), 50.0);
        assert_eq!(percentile(&xs, 99.0), 99.0);
        assert_eq!(percentile(&xs, 100.0), 100.0);
        assert_eq!(percentile(&xs, 0.0), 1.0);
        assert_eq!(percentile(&[], 99.0), 0.0);
        // Nearest rank, not interpolated: p50 of two samples is the 1st.
        assert_eq!(percentile(&[1.0, 2.0], 50.0), 1.0);
    }

    #[test]
    fn mean_over_empty_and_nonempty() {
        assert_eq!(mean(&[]), 0.0);
        assert_eq!(mean(&[1.0, 2.0, 3.0]), 2.0);
    }
}
