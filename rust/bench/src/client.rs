//! Load-generation worker: one client task owns one TCP connection.

use std::time::Instant;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::cli::{Config, Workload};
use crate::resp::{encode_command, read_reply, roundtrip, Reply};

/// SET payload size in bytes: 16 hex counter digits + filler.
const VALUE_LEN: usize = 64;
/// Counter digits at the head of a SET value.
const VALUE_COUNTER_LEN: usize = 16;
/// Stored RTT samples per client before deterministic subsampling kicks in.
const MAX_SAMPLES_PER_CLIENT: usize = 500_000;

/// 64-byte SET value: a 16-hex-digit op counter plus filler, so the payload
/// differs on every op while staying fixed size.
fn set_value(op_index: u64, out: &mut String) {
    out.clear();
    out.push_str(&format!("{:0width$x}", op_index, width = VALUE_COUNTER_LEN));
    for _ in 0..(VALUE_LEN - VALUE_COUNTER_LEN) {
        out.push('v');
    }
}

/// Effective op kind for `op_index`: `mixed` picks set on even and get on
/// odd indices (deterministic alternation on a single connection).
fn op_kind(workload: Workload, op_index: u64) -> Workload {
    match workload {
        Workload::Mixed if op_index.is_multiple_of(2) => Workload::Set,
        Workload::Mixed => Workload::Get,
        other => other,
    }
}

/// Append the command for one op to the batch buffer `buf`.
fn append_op(buf: &mut Vec<u8>, workload: Workload, key: &str, op_index: u64, value: &mut String) {
    match op_kind(workload, op_index) {
        Workload::Ping => encode_command(buf, &[b"PING"]),
        Workload::Set => {
            set_value(op_index, value);
            encode_command(buf, &[b"SET", key.as_bytes(), value.as_bytes()]);
        }
        Workload::Get => encode_command(buf, &[b"GET", key.as_bytes()]),
        Workload::Mixed => unreachable!("op_kind never returns Mixed"),
    }
}

/// Per-client aggregates, merged by the reporter after every task finishes.
pub struct ClientStats {
    /// Batch RTT samples in ms (subsampled once the cap is hit).
    pub samples: Vec<f64>,
    /// Completed commands (every reply read counts, errors included).
    pub ops: u64,
    /// Replies that started with `-` (e.g. -MOVED / -ERR).
    pub errors: u64,
    /// First error reply text, kept for the stderr diagnostic.
    pub first_error: Option<String>,
}

/// Push one batch RTT sample with deterministic subsampling: every sample
/// is stored until `MAX_SAMPLES_PER_CLIENT` is reached, afterwards only
/// every `(len/CAP + 1)`-th sample is kept (the stride grows with the
/// stored length, bounding memory; percentile accuracy degrades slightly
/// while ops/errors counters stay exact).
fn push_sample(samples: &mut Vec<f64>, seq: &mut u64, ms: f64) {
    let len = samples.len();
    if len < MAX_SAMPLES_PER_CLIENT {
        samples.push(ms);
        return;
    }
    let stride = (len / MAX_SAMPLES_PER_CLIENT + 1) as u64;
    if (*seq).is_multiple_of(stride) {
        samples.push(ms);
    }
    *seq += 1;
}

/// One client: connect, AUTH, optional warm-up SET, then batch rounds until
/// `deadline`. A batch that started before the deadline is fully drained
/// (its replies are still read), so `ops` never overcounts.
pub async fn run_client(
    cfg: &Config,
    client_id: usize,
    deadline: Instant,
) -> Result<ClientStats, String> {
    let stream = TcpStream::connect(&cfg.addr)
        .await
        .map_err(|e| format!("connect {}: {e}", cfg.addr))?;
    stream.set_nodelay(true).ok();
    let (mut rd, mut wr) = stream.into_split();

    let key = format!("bench:{client_id}");
    let mut out = Vec::with_capacity(1024);
    let mut inbox = Vec::with_capacity(4096);
    let mut value = String::with_capacity(VALUE_LEN);
    let mut stats = ClientStats {
        samples: Vec::with_capacity(4096),
        ops: 0,
        errors: 0,
        first_error: None,
    };
    let mut seq: u64 = 0;

    // AUTH first: the server rejects everything else with -ERR: NOAUTH.
    if let Reply::Error(text) = roundtrip(
        &mut wr,
        &mut rd,
        &mut inbox,
        &[b"AUTH", cfg.token.as_bytes()],
    )
    .await?
    {
        return Err(format!("AUTH rejected: {text}"));
    }

    // GET-only clients pre-populate their key once so the loop can start.
    if cfg.workload == Workload::Get {
        set_value(0, &mut value);
        let warmup = roundtrip(
            &mut wr,
            &mut rd,
            &mut inbox,
            &[b"SET", key.as_bytes(), value.as_bytes()],
        )
        .await?;
        if let Reply::Error(text) = warmup {
            return Err(format!("warm-up SET failed: {text}"));
        }
    }

    let mut op_index: u64 = 0;
    while Instant::now() < deadline {
        out.clear();
        for _ in 0..cfg.pipeline {
            append_op(&mut out, cfg.workload, &key, op_index, &mut value);
            op_index += 1;
        }
        let sent = Instant::now();
        wr.write_all(&out)
            .await
            .map_err(|e| format!("write: {e}"))?;
        let mut batch_errors = 0u64;
        for _ in 0..cfg.pipeline {
            if let Reply::Error(text) = read_reply(&mut rd, &mut inbox).await? {
                batch_errors += 1;
                if stats.first_error.is_none() {
                    stats.first_error = Some(text);
                }
            }
        }
        push_sample(
            &mut stats.samples,
            &mut seq,
            sent.elapsed().as_secs_f64() * 1000.0,
        );
        stats.ops += cfg.pipeline as u64;
        stats.errors += batch_errors;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_vary_by_op_and_stay_64_bytes() {
        let mut value = String::new();
        set_value(1, &mut value);
        assert_eq!(value.len(), 64);
        set_value(2, &mut value);
        assert_eq!(value.len(), 64);
        assert!(value.starts_with("0000000000000002"));
        set_value(0xdeadbeef, &mut value);
        assert!(value.starts_with("00000000deadbeef"));
    }

    #[test]
    fn mixed_alternates_by_parity_and_others_are_fixed() {
        assert_eq!(op_kind(Workload::Mixed, 0), Workload::Set);
        assert_eq!(op_kind(Workload::Mixed, 1), Workload::Get);
        assert_eq!(op_kind(Workload::Mixed, 7), Workload::Get);
        assert_eq!(op_kind(Workload::Ping, 9), Workload::Ping);
        assert_eq!(op_kind(Workload::Set, 9), Workload::Set);
        assert_eq!(op_kind(Workload::Get, 9), Workload::Get);
    }

    #[test]
    fn batch_encoding_matches_pipeline_count() {
        let mut buf = Vec::new();
        let mut value = String::new();
        for op in 0..3u64 {
            append_op(&mut buf, Workload::Mixed, "bench:0", op, &mut value);
        }
        // 2 SETs (even ops) + 1 GET: count the multibulk headers.
        assert_eq!(buf.iter().filter(|&&b| b == b'*').count(), 3);
        assert!(buf.starts_with(b"*3\r\n$3\r\nSET\r\n"));
        assert!(buf.windows(9).any(|w| w == b"$3\r\nGET\r\n".as_slice()));
    }

    #[test]
    fn subsampling_is_deterministic_and_bounded() {
        let mut samples = Vec::new();
        let mut seq = 0u64;
        for i in 0..(MAX_SAMPLES_PER_CLIENT + 10_000) {
            push_sample(&mut samples, &mut seq, i as f64);
        }
        assert!(samples.len() < MAX_SAMPLES_PER_CLIENT + 10_000);
        assert_eq!(
            samples[MAX_SAMPLES_PER_CLIENT - 1],
            (MAX_SAMPLES_PER_CLIENT - 1) as f64
        );
    }
}
