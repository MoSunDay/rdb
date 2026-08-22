//! Load-generation worker: one client task owns one TCP connection.

use std::time::Instant;

use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

use crate::cli::{Config, Workload};
use crate::resp::{encode_command, find_first_id, read_reply, roundtrip, Reply};

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

/// Append the command for one op to the batch buffer `buf`. Lite-stream
/// workloads target `bench_<client_id>/c`: Lite streams are mandatory
/// "parent/child" names, so the `/c` child suffix is part of the key.
fn append_op(
    buf: &mut Vec<u8>,
    workload: Workload,
    key: &str,
    client_id: usize,
    op_index: u64,
    value: &mut String,
) {
    match op_kind(workload, op_index) {
        Workload::Ping => encode_command(buf, &[b"PING"]),
        Workload::Set => {
            set_value(op_index, value);
            encode_command(buf, &[b"SET", key.as_bytes(), value.as_bytes()]);
        }
        Workload::Get => encode_command(buf, &[b"GET", key.as_bytes()]),
        Workload::Xadd => {
            let stream = format!("{key}/c");
            let payload = format!("v{op_index}");
            encode_command(
                buf,
                &[b"XADD", stream.as_bytes(), b"*", b"f", payload.as_bytes()],
            );
        }
        Workload::XReadGroup => {
            let stream = format!("{key}/c");
            let consumer = format!("c{client_id}");
            encode_command(
                buf,
                &[
                    b"XREADGROUP",
                    b"GROUP",
                    b"bench",
                    consumer.as_bytes(),
                    b"COUNT",
                    b"10",
                    b"STREAMS",
                    stream.as_bytes(),
                    b">",
                ],
            );
        }
        // The ack id only exists in the deliver reply, so xack runs its
        // own dependent round trip (see `xack_round`), never a batch.
        Workload::Xack => unreachable!("xack is never batched"),
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

    let key = format!("bench_{client_id}");
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

    // Lite consumers need the `bench` group on their stream (XREADGROUP /
    // XACK reply NOGROUP without it). Create it best-effort before the
    // measured loop: MKSTREAM tolerates a producer that has not started,
    // `$` starts at "new entries only", and the reply is ignored because
    // BUSYGROUP (already created) is fine; real problems surface in the
    // measured loop as error replies.
    if matches!(cfg.workload, Workload::XReadGroup | Workload::Xack) {
        let stream = format!("{key}/c");
        let _ = roundtrip(
            &mut wr,
            &mut rd,
            &mut inbox,
            &[
                b"XGROUP",
                b"CREATE",
                stream.as_bytes(),
                b"bench",
                b"$",
                b"MKSTREAM",
            ],
        )
        .await?;
    }

    let mut op_index: u64 = 0;
    while Instant::now() < deadline {
        // xack is a dependent round trip (ack id comes from the deliver
        // reply), so it cannot share the static pipeline-frame path.
        if cfg.workload == Workload::Xack {
            xack_round(
                &mut wr, &mut rd, &mut inbox, client_id, &mut stats, &mut seq,
            )
            .await?;
            continue;
        }
        out.clear();
        for _ in 0..cfg.pipeline {
            append_op(
                &mut out,
                cfg.workload,
                &key,
                client_id,
                op_index,
                &mut value,
            );
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

/// One `xack` iteration: `XREADGROUP GROUP bench c<id> COUNT 1` on this
/// client's Lite stream, then `XACK` the first delivered entry's id.
/// A nil/empty (or errored) deliver adds no ops — the producer may be
/// lagging — and the caller loops again; an Error deliver still counts
/// as an error. A completed deliver+ack pair counts 2 ops, one batch RTT
/// sample, and an Error on either leg is counted.
async fn xack_round(
    wr: &mut OwnedWriteHalf,
    rd: &mut OwnedReadHalf,
    inbox: &mut Vec<u8>,
    client_id: usize,
    stats: &mut ClientStats,
    seq: &mut u64,
) -> Result<(), String> {
    let stream = format!("bench_{client_id}/c");
    let consumer = format!("c{client_id}");
    let sent = Instant::now();
    let deliver = roundtrip(
        wr,
        rd,
        inbox,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"bench",
            consumer.as_bytes(),
            b"COUNT",
            b"1",
            b"STREAMS",
            stream.as_bytes(),
            b">",
        ],
    )
    .await?;
    if let Reply::Error(text) = deliver {
        stats.errors += 1;
        if stats.first_error.is_none() {
            stats.first_error = Some(text);
        }
        return Ok(());
    }
    // Nil reply, empty entries, or no id-shaped bulk: nothing to ack yet.
    let Some(id) = find_first_id(&deliver) else {
        return Ok(());
    };
    let ack = roundtrip(wr, rd, inbox, &[b"XACK", stream.as_bytes(), b"bench", &id]).await?;
    if let Reply::Error(text) = ack {
        stats.errors += 1;
        if stats.first_error.is_none() {
            stats.first_error = Some(text);
        }
    }
    push_sample(
        &mut stats.samples,
        seq,
        sent.elapsed().as_secs_f64() * 1000.0,
    );
    stats.ops += 2;
    Ok(())
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
        assert_eq!(op_kind(Workload::Xadd, 9), Workload::Xadd);
        assert_eq!(op_kind(Workload::XReadGroup, 9), Workload::XReadGroup);
        assert_eq!(op_kind(Workload::Xack, 9), Workload::Xack);
    }

    #[test]
    fn batch_encoding_matches_pipeline_count() {
        let mut buf = Vec::new();
        let mut value = String::new();
        for op in 0..3u64 {
            append_op(&mut buf, Workload::Mixed, "bench_0", 0, op, &mut value);
        }
        // 2 SETs (even ops) + 1 GET: count the multibulk headers.
        assert_eq!(buf.iter().filter(|&&b| b == b'*').count(), 3);
        assert!(buf.starts_with(b"*3\r\n$3\r\nSET\r\n"));
        assert!(buf.windows(9).any(|w| w == b"$3\r\nGET\r\n".as_slice()));
    }

    #[test]
    fn lite_stream_ops_encode_expected_frames() {
        let mut buf = Vec::new();
        let mut value = String::new();
        append_op(&mut buf, Workload::Xadd, "bench_3", 3, 7, &mut value);
        assert_eq!(
            buf,
            b"*5\r\n$4\r\nXADD\r\n$9\r\nbench_3/c\r\n$1\r\n*\r\n$1\r\nf\r\n$2\r\nv7\r\n".as_slice()
        );
        buf.clear();
        append_op(&mut buf, Workload::XReadGroup, "bench_3", 3, 0, &mut value);
        assert_eq!(
            buf,
            b"*9\r\n$10\r\nXREADGROUP\r\n$5\r\nGROUP\r\n$5\r\nbench\r\n$2\r\nc3\r\n$5\r\nCOUNT\r\n$2\r\n10\r\n$7\r\nSTREAMS\r\n$9\r\nbench_3/c\r\n$1\r\n>\r\n"
                .as_slice()
        );
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
