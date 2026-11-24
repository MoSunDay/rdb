//! Scenario performance gates (batch 11): thin wrappers that print one
//! `perf[label]` line per budget and assert it, so a full-suite log
//! doubles as the perf evidence table. The whole-scenario `scenario()`
//! ceiling is sized >=10x the slowest measured whole-scenario baseline
//! (120s vs ~5.6s, #127/#130 in the serial full-suite run). Hot-block
//! `within` budgets are regression
//! gates sized per hot block (intentionally tighter than 10x); only the
//! long-poll "parked" gates assert a LOWER bound (a server that answers
//! early is a bug, a slow host is not).

use std::time::{Duration, Instant};

/// Scenario start stamp (captured before any node spawn).
pub fn now() -> Instant {
    Instant::now()
}

/// Assert `elapsed <= max`; print the sample either way.
pub fn within(label: &'static str, started: Instant, max: Duration) -> Duration {
    let elapsed = started.elapsed();
    println!("perf[{label}] {elapsed:?} (<= {max:?})");
    assert!(
        elapsed <= max,
        "perf[{label}] took {elapsed:?}, budget {max:?}"
    );
    elapsed
}

/// Assert `elapsed >= min` (long-poll wake/timeout must really park).
pub fn at_least(label: &'static str, started: Instant, min: Duration) -> Duration {
    let elapsed = started.elapsed();
    println!("perf[{label}] {elapsed:?} (>= {min:?})");
    assert!(
        elapsed >= min,
        "perf[{label}] returned after {elapsed:?}, expected >= {min:?}"
    );
    elapsed
}

/// Assert a throughput floor for `ops` operations in `elapsed`.
pub fn rate(label: &'static str, ops: u64, elapsed: Duration, min_ops_per_sec: f64) {
    let secs = elapsed.as_secs_f64().max(1e-9);
    let got = ops as f64 / secs;
    println!("perf[{label}] {got:.1} ops/s over {elapsed:?} (>= {min_ops_per_sec})");
    assert!(
        got >= min_ops_per_sec,
        "perf[{label}] {got:.1} ops/s below {min_ops_per_sec} (ops={ops}, {elapsed:?})"
    );
}

/// Whole-scenario ceiling (every scenario must finish inside this).
pub fn scenario(label: &'static str, started: Instant) {
    within(label, started, Duration::from_secs(120));
}
