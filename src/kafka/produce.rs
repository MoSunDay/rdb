//! Produce (api 0) v0-v3: append Kafka RecordBatch v2 payloads onto the
//! mapped Lite queue.
//!
//! Version policy: v0-v2 advertised (v3 adds transactional_id; this
//! front does no transactions, so librdkafka/franz-go settle on v2).
//! Request bodies are identical across v0-v2; the v2 response adds
//! log_append_time (always -1: arrival ids carry no Kafka append time).
//!
//! Durability: acks 1/-1 both mean ONE synchronous batched fsync of the
//! whole request's records (meta + entries together) -- there is no ISR
//! (single node; documented divergence in features/kafka-front.md).
//! acks=0 writes through the detached batch path and sends NO response
//! bytes (the spec behavior: the broker does not answer acks=0), so the
//! connection keeps streaming requests.
//!
//! Field mapping (record -> Lite entry pairs; names are envelope
//! internals, see kafka-front.md):
//! - key non-null, no headers -> 2 pairs ("k", key) ("v", value)
//! - key null, no headers     -> 1 pair ("v", value)
//! - headers non-empty        -> + ("h", headers JSON); key pair omitted
//!   when the key is null
//! - a NULL key/value slot is written as the pair ("__null__", b"") so
//!   tombstones round-trip (an empty byte value stays ("v", b""))

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rocksdb::WriteBatch;

use crate::ds::{expire, latch, wait};
use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_i16, put_i32, put_i64, put_string, Reader};
use crate::kafka::mapping;
use crate::kafka::record::{parse_batch, Record};
use crate::lite::{model, offset, stat_bump};
use crate::monitor;
use crate::state::Shared;
use crate::store::ops;

/// One requested topic-partition with its raw RecordBatch bytes.
struct Target {
    partition: i32,
    records: Option<Vec<u8>>,
}

/// Decoded request: acks + topics in request order (the reply mirrors it).
struct ProduceReq {
    required_acks: i16,
    topics: Vec<(String, Vec<Target>)>,
}

/// One partition entry of the response.
struct PartitionOut {
    partition: i32,
    error: i16,
    base_offset: i64,
}

/// Handle Produce v0-v3. `Ok(None)` = acks=0: NO response frame.
pub async fn handle_produce(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Option<Vec<u8>>, String> {
    let req = parse_produce_req(body, version)?;
    let valid_acks = matches!(req.required_acks, -1 | 0 | 1);
    let mut topics_out = Vec::with_capacity(req.topics.len());
    for (name, targets) in &req.topics {
        let mut parts = Vec::with_capacity(targets.len());
        for t in targets {
            let out = if !valid_acks {
                // Kafka: any acks other than 0/1/-1 fails every partition.
                PartitionOut { partition: t.partition, error: errors::INVALID_REQUIRED_ACKS, base_offset: -1 }
            } else {
                match produce_one(shared, name, t, req.required_acks == 0).await {
                    Ok(base) => PartitionOut { partition: t.partition, error: errors::NONE, base_offset: base },
                    Err(code) => PartitionOut { partition: t.partition, error: code, base_offset: -1 },
                }
            };
            parts.push(out);
        }
        topics_out.push((name.clone(), parts));
    }
    if req.required_acks == 0 {
        return Ok(None);
    }
    Ok(Some(produce_body(version, topics_out)))
}

/// Append one target's records; `Ok(base_offset)` = the pre-append meta
/// len. All records of the batch land in ONE WriteBatch + one fsync
/// (acks 1/-1) or one detached write (acks 0).
async fn produce_one(
    shared: &Shared,
    topic: &str,
    target: &Target,
    fire_and_forget: bool,
) -> Result<i64, i16> {
    mapping::validate_topic(topic.as_bytes())?;
    let parent = topic.as_bytes().to_vec();
    let prefix = hash::slot_with_prefix(&parent).1;
    let child = mapping::partition_queue(&shared.store, &prefix, &parent, target.partition)
        .map_err(|_| errors::UNKNOWN_SERVER_ERROR)?
        .ok_or(errors::UNKNOWN_TOPIC_OR_PARTITION)?;
    let raw = target
        .records
        .as_deref()
        .ok_or(errors::CORRUPT_MESSAGE)?; // null records: nothing to append
    let batch = parse_batch(raw).map_err(classify_parse_err)?;
    let fields: Vec<Vec<(Vec<u8>, Vec<u8>)>> =
        batch.records.iter().map(record_fields).collect();

    let mut stream = parent.clone();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    let mkey = model::meta_key(&prefix, &stream);
    let _guard = latch::lock(&shared.latch, &mkey).await;
    let now = expire::now_ms();
    let read = model::read_meta(&shared.store, &prefix, &stream, Some(shared.lite.as_ref()))
        .map_err(|_| errors::UNKNOWN_SERVER_ERROR)?;
    let (meta, fresh) = match read {
        model::MetaRead::Live(m) => (m, false),
        _ => (
            model::MetaPayload {
                created_ms: now,
                ..Default::default()
            },
            true,
        ),
    };
    let base = meta.len as i64;
    if fields.is_empty() {
        return Ok(base); // empty batch: a no-op probe, no meta touch
    }
    // Ids come from the arrival clock (auto_id bumps seq within a ms);
    // Kafka record timestamps are not carried into entry ids.
    let mut ids = Vec::with_capacity(fields.len());
    let mut last = (!fresh).then_some(meta.last_id());
    for _ in 0..fields.len() {
        let id = model::auto_id(last, now).ok_or(errors::UNKNOWN_SERVER_ERROR)?;
        ids.push(id);
        last = Some(id);
    }
    let mut next = meta.clone();
    next.last_ms = ids.last().map(|i| i.ms).unwrap_or(0);
    next.last_seq = ids.last().map(|i| i.seq).unwrap_or(0);
    next.len += ids.len() as u64;
    let old_expire = if fresh {
        0
    } else {
        model::current_expire(&shared.store, &prefix, &stream)
    };
    let new_expire = match idle_deadline(&next, now) {
        Some(d) => d,
        None => return Err(errors::UNKNOWN_SERVER_ERROR),
    };

    let mut wb = WriteBatch::default();
    wb.put(&mkey, model::encode_meta_at(&next, new_expire));
    for (id, pairs) in ids.iter().zip(&fields) {
        let flat: Vec<(&[u8], &[u8])> =
            pairs.iter().map(|(f, v)| (f.as_slice(), v.as_slice())).collect();
        wb.put(model::entry_key(&prefix, &stream, *id), model::encode_entry(&flat));
    }
    expire::set_ttl_entries(&mut wb, &prefix, mkey.clone(), old_expire, new_expire);
    if fire_and_forget {
        ops::spawn_batch_write(Arc::clone(&shared.store), wb);
    } else if ops::batch_write_async(Arc::clone(&shared.store), wb).await.is_err() {
        return Err(errors::UNKNOWN_SERVER_ERROR);
    }

    if fresh {
        shared.lite.stats.streams_live.fetch_add(1, Ordering::Relaxed);
        offset::remove_stream(&shared.lite.offsets, &stream);
    }
    stat_bump(&shared.lite.stats.messages, ids.len() as u64);
    monitor::observe_lite_message(&shared.monitor, "add", ids.len() as u64);
    // Wake blocked readers parked on this queue's meta AND the parent's
    // (same hub-key discipline as the XADD path).
    wait::notify(&shared.wait_hub, &mkey);
    wait::notify(&shared.wait_hub, &model::meta_key(&prefix, &parent));
    Ok(base)
}

/// Idle deadline of the post-append meta (0 = none); `None` = overflow
/// (reject instead of arming a wrapped, instantly-past deadline).
fn idle_deadline(meta: &model::MetaPayload, now: u64) -> Option<u64> {
    if meta.idle_ms == 0 {
        return Some(0);
    }
    now.max(meta.last_ms).checked_add(meta.idle_ms)
}

/// RecordBatch parse failure -> Kafka error code (parse_batch checks in
/// spec order: magic, CRC, compression; message prefixes are stable and
/// unit-anchored).
pub(super) fn classify_parse_err(e: String) -> i16 {
    if e.contains("magic") {
        errors::UNSUPPORTED_VERSION
    } else if e.contains("compressed") {
        errors::UNSUPPORTED_COMPRESSION_TYPE
    } else {
        errors::CORRUPT_MESSAGE
    }
}

/// Record -> Lite field pairs (see the module map).
pub(super) fn record_fields(rec: &Record) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(3);
    if let Some(k) = &rec.key {
        out.push((b"k".to_vec(), k.clone()));
    }
    match &rec.value {
        Some(v) => out.push((b"v".to_vec(), v.clone())),
        None => out.push((b"__null__".to_vec(), Vec::new())),
    }
    if !rec.headers.is_empty() {
        out.push((b"h".to_vec(), headers_json(&rec.headers).into_bytes()));
    }
    out
}

/// Headers as `[{"n":name,"v":null},{"n":name,"v":"<hex>"}]` (byte
/// values hex-encoded; names are UTF-8 strings per the wire format).
fn headers_json(headers: &[(String, Option<Vec<u8>>)]) -> String {
    let mut out = String::from("[");
    for (i, (name, val)) in headers.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let name = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string());
        match val {
            None => out.push_str(&format!("{{\"n\":{name},\"v\":null}}")),
            Some(v) => out.push_str(&format!("{{\"n\":{name},\"v\":\"{}\"}}", hex::encode(v))),
        }
    }
    out.push(']');
    out
}

fn parse_produce_req(body: &mut Reader<'_>, version: i16) -> Result<ProduceReq, String> {
    if version >= 3 {
        // transactional_id (v3+): this front is not transactional, the
        // id is parsed and ignored.
        body.nullable_string().ok_or("malformed produce request")?;
    }
    let required_acks = body.i16().ok_or("malformed produce request")?;
    let _timeout_ms = body.i32().ok_or("malformed produce request")?;
    let n_topics = body.array_len().ok_or("malformed produce request")?.unwrap_or(0);
    let mut topics = Vec::with_capacity(n_topics.min(1024));
    for _ in 0..n_topics {
        let name = body.string().ok_or("malformed produce topic")?;
        let n_parts = body.array_len().ok_or("malformed produce request")?.unwrap_or(0);
        let mut targets = Vec::with_capacity(n_parts.min(1024));
        for _ in 0..n_parts {
            let partition = body.i32().ok_or("malformed produce partition")?;
            let records = body
                .bytes()
                .ok_or("malformed produce records")?
                .map(|b| b.to_vec());
            targets.push(Target { partition, records });
        }
        topics.push((name, targets));
    }
    Ok(ProduceReq { required_acks, topics })
}

/// Response body v0-v2: responses[topic[partition,error,base_offset
/// (,log_append_time on v2)]].
fn produce_body(version: i16, topics: Vec<(String, Vec<PartitionOut>)>) -> Vec<u8> {
    let mut out = Vec::new();
    put_array_len(&mut out, topics.len());
    for (name, parts) in topics {
        put_string(&mut out, &name);
        put_array_len(&mut out, parts.len());
        for p in parts {
            put_i32(&mut out, p.partition);
            put_i16(&mut out, p.error);
            put_i64(&mut out, p.base_offset);
            if version >= 2 {
                put_i64(&mut out, -1); // log_append_time
            }
        }
    }
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms trails the responses array
    }
    out
}
