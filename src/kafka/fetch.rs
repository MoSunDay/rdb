//! Fetch (api 1) v0-v10: read RecordBatches back out of the mapped Lite
//! queue, with Kafka long-poll semantics.
//!
//! Version policy: v0-v10 advertised (v10 is the last classic-framing
//! version; v11 turns flexible). Request order follows the OFFICIAL
//! schema (max_bytes after min_bytes from v3, NOT the trailing position
//! a task sketch once suggested): replica_id, max_wait, min_bytes,
//! [v3+ max_bytes], [v4+ isolation_level], [v7+ session_id/epoch --
//! always 0/0, no incremental sessions], topics[topic, partitions[
//! partition, [v9+ current_leader_epoch], fetch_offset, [v5+
//! log_start_offset], partition_max_bytes]],
//! [v7+ forgotten_topics_data -- parsed and ignored], ([v11+ rack_id --
//! never reached]).
//!
//! Offsets are entry ORDINALS: `fetch_offset > len` answers
//! OFFSET_OUT_OF_RANGE, `== len` an empty record set (the EOF poll),
//! missing topic/partition UNKNOWN_TOPIC_OR_PARTITION (this front never
//! auto-creates). high_watermark/last_stable_offset = meta `len`,
//! log_start_offset = 0, aborted_transactions = null, preferred_read_
//! replica = -1 (a v11+ field, absent from classic v0-v10): a single
//! broker has no epochs/replicas/fencing.
//!
//! Long-poll: when max_wait > 0 AND no partition errored AND the scan
//! produced fewer record bytes than min_bytes (<=0 read as 1), the
//! handler parks on the DISTINCT child-stream meta keys (register ->
//! scan -> park-while-registered, the `lite::park_wait` lost-notify
//! pattern inlined -- that module is lite-internal) and re-scans on
//! every wake. DEVIATION (documented): min_bytes is only honored for
//! the all-empty case -- once ANY partition has records the response
//! returns immediately instead of accumulating across partitions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bytes, put_i16, put_i32, put_i64, put_null_array_len, put_string, Reader,
};
use crate::kafka::fetch_records::collect_records;
use crate::kafka::mapping;
use crate::lite::model;
use crate::state::Shared;
use crate::{ds::wait, park};

/// One park slice cap (24h): renewable, mirrors `lite::park_wait`.
const MAX_SLICE_MS: u64 = 86_400_000;
/// Default when the wire omits a budget (schema always sends one, but a
/// hostile 0 must not mean "unbounded").
const MIN_PARTITION_BUDGET: i32 = 1;

/// One requested partition.
struct PartReq {
    partition: i32,
    fetch_offset: i64,
    partition_max_bytes: i32,
}

/// One requested topic with its partitions.
struct TopicReq {
    name: String,
    parts: Vec<PartReq>,
}

struct FetchReq {
    max_wait_time: i32,
    min_bytes: i32,
    max_bytes: Option<i32>,
    topics: Vec<TopicReq>,
}

/// One resolved partition: the physical child stream backing it.
struct Target {
    partition: i32,
    fetch_offset: i64,
    budget: i32,
    /// `None` = errored (unknown topic/partition): no records, no park.
    stream: Option<(Vec<u8>, Vec<u8>)>, // (prefix, stream name)
}

struct TopicTargets {
    name: String,
    targets: Vec<Target>,
}

/// One response partition.
struct PartOut {
    partition: i32,
    error: i16,
    hwm: i64,
    records: Vec<u8>,
}

/// Handle Fetch v0-v10. Returns the response body plus the wall time
/// spent parked in long-poll waits: `conn::process` EXCLUDES the park
/// from the api-latency histogram (parking is the client's own
/// max_wait, not handler work -- counting it pushed every long poll
/// toward the +Inf bucket).
pub async fn handle_fetch(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<(Vec<u8>, u64), String> {
    let req = parse_fetch(body, version)?;
    let mut global_left: Option<i64> = req.max_bytes.map(|m| m.max(0) as i64);
    let topics = resolve_targets(shared, &req, &mut global_left);
    let min = req.min_bytes.max(1) as usize;
    let deadline = if req.max_wait_time > 0 {
        Instant::now().checked_add(Duration::from_millis(
            req.max_wait_time.min(MAX_SLICE_MS as i32) as u64,
        ))
    } else {
        None
    };
    // Distinct park keys: child-stream meta keys of the live targets.
    let mut keys: Vec<Vec<u8>> = topics
        .iter()
        .flat_map(|t| t.targets.iter().filter_map(|g| g.stream.as_ref()))
        .map(|(prefix, stream)| model::meta_key(prefix, stream))
        .collect();
    keys.sort();
    keys.dedup();

    let mut parked_ms: u64 = 0;
    loop {
        // Register BEFORE deciding (closes the lost-notify race): a
        // produce that lands after this scan still wakes the park.
        let waiter = Arc::new(wait::new_waiter());
        for k in &keys {
            wait::register_shared(&shared.wait_hub, k, &waiter);
        }
        let mut parts: Vec<(String, Vec<PartOut>)> = Vec::with_capacity(topics.len());
        let mut total = 0usize;
        let mut live = false;
        for t in &topics {
            let mut outs = Vec::with_capacity(t.targets.len());
            for g in &t.targets {
                let out = match &g.stream {
                    None => PartOut {
                        partition: g.partition,
                        error: errors::UNKNOWN_TOPIC_OR_PARTITION,
                        hwm: -1,
                        records: Vec::new(),
                    },
                    Some((prefix, stream)) => {
                        live = true;
                        read_partition(shared, prefix, stream, g.partition, g.fetch_offset, g.budget)?
                    }
                };
                total += out.records.len();
                outs.push(out);
            }
            parts.push((t.name.clone(), outs));
        }
        let spent = deadline.is_none_or(|d| Instant::now() >= d);
        if total >= min || spent || keys.is_empty() || !live {
            for k in &keys {
                wait::unregister(&shared.wait_hub, k, &waiter);
            }
            return Ok((fetch_body(version, &parts), parked_ms));
        }
        // Renewable slice: at most MAX_SLICE and never past the deadline.
        let now = Instant::now();
        let slice = deadline
            .map(|d| (d - now).min(Duration::from_millis(MAX_SLICE_MS)))
            .unwrap_or(Duration::from_millis(MAX_SLICE_MS));
        let w = Arc::clone(&waiter);
        let parked = Instant::now();
        let _ = park::park(move || wait::wait(&w, slice)).await;
        parked_ms += parked.elapsed().as_millis() as u64;
        for k in &keys {
            wait::unregister(&shared.wait_hub, k, &waiter);
        }
    }
}

/// Parse a Fetch request (classic framing, v0-v10).
fn parse_fetch(body: &mut Reader<'_>, version: i16) -> Result<FetchReq, String> {
    let bad = || "malformed fetch request".to_string();
    let _replica_id = body.i32().ok_or_else(bad)?;
    let max_wait_time = body.i32().ok_or_else(bad)?;
    let min_bytes = body.i32().ok_or_else(bad)?;
    let max_bytes = (version >= 3).then(|| body.i32().ok_or_else(bad)).transpose()?;
    if version >= 4 {
        let _isolation_level = body.i8().ok_or_else(bad)?;
    }
    if version >= 7 {
        let _session_id = body.i32().ok_or_else(bad)?;
        let _session_epoch = body.i32().ok_or_else(bad)?;
    }
    let n_topics = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut topics = Vec::with_capacity(n_topics.min(1024));
    for _ in 0..n_topics {
        let name = body.string().ok_or_else(bad)?;
        let n_parts = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        let mut parts = Vec::with_capacity(n_parts.min(1024));
        for _ in 0..n_parts {
            let partition = body.i32().ok_or_else(bad)?;
            if version >= 9 {
                let _current_leader_epoch = body.i32().ok_or_else(bad)?;
            }
            let fetch_offset = body.i64().ok_or_else(bad)?;
            if version >= 5 {
                let _log_start_offset = body.i64().ok_or_else(bad)?;
            }
            let partition_max_bytes = body.i32().ok_or_else(bad)?;
            parts.push(PartReq {
                partition,
                fetch_offset,
                partition_max_bytes,
            });
        }
        topics.push(TopicReq { name, parts });
    }
    if version >= 7 {
        // forgotten_topics_data: parsed (shape-validated) and ignored --
        // there are no incremental fetch sessions to forget from.
        let n = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        for _ in 0..n {
            let _topic = body.string().ok_or_else(bad)?;
            let n_parts = body.array_len().ok_or_else(bad)?.unwrap_or(0);
            for _ in 0..n_parts {
                let _partition = body.i32().ok_or_else(bad)?;
            }
        }
    }
    Ok(FetchReq {
        max_wait_time,
        min_bytes,
        max_bytes,
        topics,
    })
}

/// Resolve every requested partition to its child stream (or an error
/// target). `global_left` is the v3+ request-wide records budget every
/// partition draws from.
fn resolve_targets(
    shared: &Shared,
    req: &FetchReq,
    global_left: &mut Option<i64>,
) -> Vec<TopicTargets> {
    req.topics
        .iter()
        .map(|t| {
            let targets = t
                .parts
                .iter()
                .map(|p| {
                    let stream = resolve_stream(shared, &t.name, p.partition);
                    let budget = match (stream.is_some(), global_left.as_mut()) {
                        (true, Some(left)) => {
                            let take = (*left).min(p.partition_max_bytes.max(0) as i64);
                            *left = (*left - take).max(0);
                            take.max(MIN_PARTITION_BUDGET as i64) as i32
                        }
                        (true, None) => p.partition_max_bytes.max(MIN_PARTITION_BUDGET),
                        (false, _) => 0,
                    };
                    Target {
                        partition: p.partition,
                        fetch_offset: p.fetch_offset,
                        budget,
                        stream,
                    }
                })
                .collect();
            TopicTargets {
                name: t.name.clone(),
                targets,
            }
        })
        .collect()
}

/// `(prefix, stream)` of a known topic-partition; `None` = unknown.
fn resolve_stream(
    shared: &Shared,
    topic: &str,
    partition: i32,
) -> Option<(Vec<u8>, Vec<u8>)> {
    mapping::validate_topic(topic.as_bytes()).ok()?;
    let parent = topic.as_bytes().to_vec();
    let prefix = hash::slot_with_prefix(&parent).1;
    let child = mapping::partition_queue(&shared.store, &prefix, &parent, partition).ok()??;
    let mut stream = parent;
    stream.push(b'/');
    stream.extend_from_slice(&child);
    Some((prefix, stream))
}

/// Read one partition: the records batch + high watermark.
fn read_partition(
    shared: &Shared,
    prefix: &[u8],
    stream: &[u8],
    partition: i32,
    fetch_offset: i64,
    budget: i32,
) -> Result<PartOut, String> {
    let len = mapping::latest_ordinal(&shared.store, prefix, stream)?
        .ok_or_else(|| "stream vanished".to_string())?;
    if fetch_offset < 0 || fetch_offset > len as i64 {
        return Ok(PartOut {
            partition,
            error: errors::OFFSET_OUT_OF_RANGE,
            hwm: len as i64,
            records: Vec::new(),
        });
    }
    let records = collect_records(
        &shared.store,
        prefix,
        stream,
        fetch_offset as u64,
        budget.max(MIN_PARTITION_BUDGET) as usize,
    )?;
    Ok(PartOut {
        partition,
        error: errors::NONE,
        hwm: len as i64,
        records,
    })
}


/// Response body v0-v10 (official FetchResponse field order:
/// throttle v1+, error_code v7+ BEFORE session_id v7+, and
/// preferred_read_replica v11+ -- never written for classic v10).
fn fetch_body(version: i16, topics: &[(String, Vec<PartOut>)]) -> Vec<u8> {
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    if version >= 7 {
        put_i16(&mut out, 0); // top-level error_code
        put_i32(&mut out, 0); // session_id (no incremental sessions)
    }
    put_array_len(&mut out, topics.len());
    for (name, parts) in topics {
        put_string(&mut out, name);
        put_array_len(&mut out, parts.len());
        for p in parts {
            put_i32(&mut out, p.partition);
            put_i16(&mut out, p.error);
            put_i64(&mut out, p.hwm);
            if version >= 4 {
                put_i64(&mut out, p.hwm); // last_stable_offset
            }
            if version >= 5 {
                put_i64(&mut out, 0); // log_start_offset
            }
            if version >= 4 {
                put_null_array_len(&mut out); // aborted_transactions: null
            }
            if version >= 11 {
                put_i32(&mut out, -1); // preferred_read_replica
            }
            put_bytes(&mut out, &p.records);
        }
    }
    out
}
