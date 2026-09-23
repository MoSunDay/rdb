//! Kafka-front load loops: the `kafka-prod` (Produce v2, acks=1) and
//! `kafka-fetch` (Fetch v4 tail follow) workloads.
//!
//! Mirrors the RESP client: one client = one task = one TCP connection,
//! closed-loop (one request in flight per connection, matched by
//! correlation id). `ops` counts RECORDS -- a produce request carries
//! `--batch` records, a fetch reply may carry many -- so the reported
//! ops/s is records/s and the two workloads' counts are directly
//! comparable. The topic partition `<topic>/q0` is pre-created over
//! RESP (`ensure_topic`, XADD seed) because the kafka front never
//! auto-creates topics.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::cli::{Config, Workload};
use crate::client::{push_sample, set_value, ClientStats};
use crate::kafka_reply;
use crate::kafka_wire;
use crate::resp;

/// Client id in the request header (diagnostics only, per connection).
const CLIENT_ID: &str = "rdb-bench";
/// acks=1: one synchronous batched fsync per request server-side.
const ACKS: i16 = 1;
const PRODUCE_TIMEOUT_MS: i32 = 5_000;
/// Fetch long-poll budget: the front parks up to this long when the
/// partition tail is empty, so this IS the tail-follow wait.
const FETCH_MAX_WAIT_MS: i32 = 500;
const FETCH_MIN_BYTES: i32 = 1;
const FETCH_MAX_BYTES: i32 = 8 * 1024 * 1024;
const FETCH_PARTITION_MAX_BYTES: i32 = 4 * 1024 * 1024;
/// Largest response frame accepted (mirrors the server's own cap).
const MAX_FRAME_BYTES: usize = 100 * 1024 * 1024;

/// Wall-clock ms since the epoch (RecordBatch first_timestamp).
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Error-code label for the first_error diagnostic.
fn error_label(code: i16) -> String {
    match code {
        0 => "NONE".to_string(),
        1 => "OFFSET_OUT_OF_RANGE".to_string(),
        3 => "UNKNOWN_TOPIC_OR_PARTITION".to_string(),
        other => format!("kafka error {other}"),
    }
}

/// Count one error reply (or client-side assertion failure) on `stats`.
fn note_error(stats: &mut ClientStats, text: String) {
    stats.errors += 1;
    if stats.first_error.is_none() {
        stats.first_error = Some(text);
    }
}

/// Connect to RESP and XADD one seed entry into `<topic>/q0`, creating
/// the topic partition the kafka workloads target.
pub async fn ensure_topic(cfg: &Config) -> Result<(), String> {
    let stream = TcpStream::connect(&cfg.addr)
        .await
        .map_err(|e| format!("connect {}: {e}", cfg.addr))?;
    stream.set_nodelay(true).ok();
    let (mut rd, mut wr) = stream.into_split();
    let mut inbox = Vec::with_capacity(512);
    if let resp::Reply::Error(text) = resp::roundtrip(
        &mut wr,
        &mut rd,
        &mut inbox,
        &[b"AUTH", cfg.token.as_bytes()],
    )
    .await?
    {
        return Err(format!("AUTH rejected: {text}"));
    }
    let stream_name = format!("{}/q0", cfg.topic);
    let seeded = resp::roundtrip(
        &mut wr,
        &mut rd,
        &mut inbox,
        &[b"XADD", stream_name.as_bytes(), b"*", b"f", b"1"],
    )
    .await?;
    if let resp::Reply::Error(text) = seeded {
        return Err(format!("XADD {stream_name}: {text}"));
    }
    Ok(())
}

/// Length-prefixed request write.
async fn write_frame(sock: &mut TcpStream, payload: &[u8]) -> Result<(), String> {
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    sock.write_all(&framed)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// Length-prefixed response read.
async fn read_frame(sock: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut lenb = [0u8; 4];
    sock.read_exact(&mut lenb)
        .await
        .map_err(|e| format!("read frame length: {e}"))?;
    let len = i32::from_be_bytes(lenb);
    if len < 0 || len as usize > MAX_FRAME_BYTES {
        return Err(format!("bad response frame length {len}"));
    }
    let mut payload = vec![0u8; len as usize];
    sock.read_exact(&mut payload)
        .await
        .map_err(|e| format!("read frame body: {e}"))?;
    Ok(payload)
}

/// One kafka client task: connect to `--host`, dispatch to the workload
/// loop, return the aggregates (same contract as `client::run_client`).
pub async fn run_kafka_client(
    cfg: &Config,
    client_id: usize,
    deadline: Instant,
) -> Result<ClientStats, String> {
    let mut sock = TcpStream::connect(&cfg.host)
        .await
        .map_err(|e| format!("connect {}: {e}", cfg.host))?;
    sock.set_nodelay(true).ok();
    let mut stats = ClientStats {
        samples: Vec::with_capacity(4096),
        ops: 0,
        errors: 0,
        bytes: 0,
        first_error: None,
    };
    let cid = format!("{CLIENT_ID}-c{client_id}");
    match cfg.workload {
        Workload::KafkaProd => produce_loop(&mut sock, cfg, &cid, deadline, &mut stats).await?,
        Workload::KafkaFetch => fetch_loop(&mut sock, cfg, &cid, deadline, &mut stats).await?,
        _ => unreachable!("non-kafka workload dispatched to the kafka client"),
    }
    Ok(stats)
}

/// Produce v2 closed loop: one RecordBatch of `--batch` records per
/// request, acks=1. Keys are 16-hex op counters, values reuse the SET
/// payload shape (16-hex counter + filler, 64 bytes). base_offset must
/// never regress (single partition, serialized connection).
async fn produce_loop(
    sock: &mut TcpStream,
    cfg: &Config,
    cid: &str,
    deadline: Instant,
    stats: &mut ClientStats,
) -> Result<(), String> {
    let mut corr: i32 = 1;
    let mut seq: u64 = 0;
    let mut op_index: u64 = 0;
    let mut last_base: i64 = -1;
    let mut keys: Vec<String> = Vec::with_capacity(cfg.batch);
    let mut values: Vec<String> = Vec::with_capacity(cfg.batch);
    let mut batch = Vec::with_capacity(cfg.batch * 96 + 128);
    while Instant::now() < deadline {
        keys.clear();
        values.clear();
        for _ in 0..cfg.batch {
            keys.push(format!("{:016x}", op_index));
            let mut value = String::with_capacity(64);
            set_value(op_index, &mut value);
            values.push(value);
            op_index += 1;
        }
        let records: Vec<kafka_wire::BenchRecord> = keys
            .iter()
            .zip(values.iter())
            .map(|(k, v)| kafka_wire::BenchRecord {
                key: k.as_bytes(),
                value: v.as_bytes(),
            })
            .collect();
        kafka_wire::build_batch(now_ms(), &records, &mut batch);
        let req =
            kafka_wire::produce_request(corr, cid, &cfg.topic, ACKS, PRODUCE_TIMEOUT_MS, &batch);
        let sent = Instant::now();
        write_frame(sock, &req).await?;
        let payload = read_frame(sock).await?;
        let out = kafka_reply::parse_produce(&payload, corr)?;
        push_sample(
            &mut stats.samples,
            &mut seq,
            sent.elapsed().as_secs_f64() * 1000.0,
        );
        stats.ops += cfg.batch as u64;
        stats.bytes += batch.len() as u64;
        if out.error != 0 {
            note_error(stats, error_label(out.error));
        }
        if out.base_offset < last_base {
            note_error(
                stats,
                format!(
                    "base_offset regression: {} after {last_base}",
                    out.base_offset
                ),
            );
        } else {
            last_base = out.base_offset;
        }
        corr += 1;
    }
    Ok(())
}

/// Fetch v4 tail follow: read partition 0 from a local cursor, advance
/// by the records counted in each reply. At the tail the server parks
/// up to FETCH_MAX_WAIT_MS (min_bytes=1), which is the idle wait; an
/// OFFSET_OUT_OF_RANGE (log shrank) re-arms the cursor at the reported
/// high watermark instead of failing the run.
async fn fetch_loop(
    sock: &mut TcpStream,
    cfg: &Config,
    cid: &str,
    deadline: Instant,
    stats: &mut ClientStats,
) -> Result<(), String> {
    let mut corr: i32 = 1;
    let mut seq: u64 = 0;
    let mut offset: i64 = 0;
    while Instant::now() < deadline {
        let req = kafka_wire::fetch_request(
            corr,
            cid,
            &cfg.topic,
            offset,
            FETCH_MAX_WAIT_MS,
            FETCH_MIN_BYTES,
            FETCH_MAX_BYTES,
            FETCH_PARTITION_MAX_BYTES,
        );
        let sent = Instant::now();
        write_frame(sock, &req).await?;
        let payload = read_frame(sock).await?;
        let out = kafka_reply::parse_fetch(&payload, corr)?;
        push_sample(
            &mut stats.samples,
            &mut seq,
            sent.elapsed().as_secs_f64() * 1000.0,
        );
        match out.error {
            0 => {
                let n = kafka_wire::count_records(&out.records)?;
                stats.ops += n;
                stats.bytes += out.records.len() as u64;
                offset += n as i64;
            }
            1 => {
                note_error(stats, error_label(1));
                offset = out.hwm.max(0);
            }
            code => note_error(stats, error_label(code)),
        }
        corr += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_labels_cover_common_codes() {
        assert_eq!(error_label(0), "NONE");
        assert_eq!(error_label(1), "OFFSET_OUT_OF_RANGE");
        assert_eq!(error_label(3), "UNKNOWN_TOPIC_OR_PARTITION");
        assert_eq!(error_label(17), "kafka error 17");
    }

    #[test]
    fn note_error_keeps_first_text_only() {
        let mut stats = ClientStats {
            samples: Vec::new(),
            ops: 0,
            errors: 0,
            bytes: 0,
            first_error: None,
        };
        note_error(&mut stats, "first".to_string());
        note_error(&mut stats, "second".to_string());
        assert_eq!(stats.errors, 2);
        assert_eq!(stats.first_error.as_deref(), Some("first"));
    }
}
