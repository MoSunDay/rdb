//! The two constant-answer admin APIs of the P3 backfill:
//!
//! - DescribeConfigs (32) v0-v3, classic framing only (v4 is
//!   flexible/tagged -- above the cap; librdkafka's own window is
//!   0-1, so it lands on v1). STUB by design: resource_type TOPIC(2)
//!   answers a minimal static-but-sane set (the AdminClient
//!   startup-facing constants below -- Lite streams carry no topic
//!   configs, so there is nothing dynamic to report); every other
//!   resource type answers INVALID_REQUEST. An unknown topic answers
//!   UNKNOWN_TOPIC_OR_PARTITION like the broker does.
//! - OffsetForLeaderEpoch (23) v0-v3, classic only (v4 flexible;
//!   librdkafka uses exactly v2). Single-node semantics: the broker
//!   reports NO epochs anywhere (Fetch's own posture), so every
//!   partition answers error 0 with leader_epoch = -1 ("unknown",
//!   which makes KIP-320-aware clients skip truncation) and
//!   end_offset = the current log end offset (the same meta-`len`
//!   source Fetch uses for the high watermark).

use crate::hash;
use crate::kafka::catalog;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_bool, put_i16, put_i32, put_i64, put_i8, put_nullable_string, put_string,
    Reader,
};
use crate::kafka::mapping;
use crate::state::Shared;

/// ConfigResource type: TOPIC.
const RESOURCE_TYPE_TOPIC: i8 = 2;
/// ConfigSource value for a default-config entry (librdkafka maps it
/// onto its is_default view; v0 responses carry is_default directly).
const CONFIG_SOURCE_DEFAULT: i8 = 5;

/// The minimal static-but-sane TOPIC config set: Kafka's own defaults
/// where a default exists, so AdminClient tooling renders something
/// recognizable. retention.ms mirrors Kafka's 7-day default (Lite
/// streams carry no retention; this is a documented constant, not a
/// live setting).
const TOPIC_DEFAULT_CONFIGS: &[(&str, &str)] = &[
    ("cleanup.policy", "delete"),
    ("retention.ms", "604800000"),
    ("retention.bytes", "-1"),
    ("min.insync.replicas", "1"),
];

/// DescribeConfigs v0-v3: resources[type, name, keys[]] in, throttle +
/// results[error, message, type, name, entries[]] out (request order).
pub fn handle_describe_configs(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed describeconfigs request".to_string();
    let n = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut resources = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let rtype = body.i8().ok_or_else(bad)?;
        let name = body.string().ok_or_else(bad)?;
        let mut keys = Vec::new();
        for _ in 0..body.array_len().ok_or_else(bad)?.unwrap_or(0) {
            keys.push(body.string().ok_or_else(bad)?);
        }
        resources.push((rtype, name, keys));
    }
    if version >= 1 {
        body.boolean().ok_or_else(bad)?; // include_synonyms (no synonyms exist)
    }
    if version >= 3 {
        body.boolean().ok_or_else(bad)?; // include_documentation (none emitted)
    }
    let mut out = Vec::new();
    put_i32(&mut out, 0); // throttle_time_ms (v0+)
    put_array_len(&mut out, resources.len());
    for (rtype, name, keys) in &resources {
        let (code, msg) = if *rtype != RESOURCE_TYPE_TOPIC {
            (
                errors::INVALID_REQUEST,
                Some("only TOPIC resources are supported"),
            )
        } else {
            match describe_topic(shared, name) {
                Ok(()) => (errors::NONE, None),
                Err(code) => (code, Some("no such topic")),
            }
        };
        put_i16(&mut out, code);
        put_nullable_string(&mut out, msg);
        put_i8(&mut out, *rtype);
        put_string(&mut out, name);
        let entries: Vec<&(&str, &str)> = TOPIC_DEFAULT_CONFIGS
            .iter()
            .filter(|(k, _)| keys.is_empty() || keys.iter().any(|want| want == k))
            .collect();
        put_array_len(&mut out, entries.len());
        for (k, v) in entries {
            put_string(&mut out, k);
            put_nullable_string(&mut out, Some(v));
            put_bool(&mut out, false); // read_only
            if version == 0 {
                put_bool(&mut out, true); // is_default
            } else {
                put_i8(&mut out, CONFIG_SOURCE_DEFAULT); // config_source
            }
            put_bool(&mut out, false); // is_sensitive
            if version >= 1 {
                put_array_len(&mut out, 0); // synonyms
            }
            if version >= 3 {
                put_i8(&mut out, 0); // config_type: UNKNOWN
                put_nullable_string(&mut out, None); // documentation
            }
        }
    }
    Ok(out)
}

/// Topic-existence gate for a DescribeConfigs TOPIC resource:
/// `Err(INVALID_TOPIC_EXCEPTION | UNKNOWN_TOPIC_OR_PARTITION)`.
fn describe_topic(shared: &Shared, name: &str) -> Result<(), i16> {
    mapping::validate_topic(name.as_bytes())?;
    match catalog::partitions_of(&shared.store, name.as_bytes()) {
        Ok(p) if !p.is_empty() => Ok(()),
        Ok(_) => Err(errors::UNKNOWN_TOPIC_OR_PARTITION),
        Err(_) => Err(errors::UNKNOWN_SERVER_ERROR),
    }
}

/// OffsetForLeaderEpoch v0-v3. Request: [v3 replica_id] topics[topic,
/// partitions[partition, current_leader_epoch(v2+), leader_epoch]];
/// response: [v2 throttle] topics[topic, partitions[error, partition,
/// leader_epoch(v1+), end_offset]] -- all classic (v4 is flexible).
pub fn handle_offset_for_leader_epoch(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed offsetforleaderepoch request".to_string();
    if version >= 3 {
        body.i32().ok_or_else(bad)?; // replica_id (-1 = client): single node, ignored
    }
    let n_topics = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut out = Vec::new();
    if version >= 2 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_array_len(&mut out, n_topics);
    for _ in 0..n_topics {
        let name = body.string().ok_or_else(bad)?;
        let n_parts = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        put_string(&mut out, &name);
        put_array_len(&mut out, n_parts);
        for _ in 0..n_parts {
            let partition = body.i32().ok_or_else(bad)?;
            if version >= 2 {
                body.i32().ok_or_else(bad)?; // current_leader_epoch: no epochs exist
            }
            body.i32().ok_or_else(bad)?; // leader_epoch (the epoch being probed)
            let (code, end) = epoch_answer(shared, &name, partition)?;
            put_i16(&mut out, code);
            put_i32(&mut out, partition);
            if version >= 1 {
                put_i32(&mut out, -1); // leader_epoch: unknown (no epochs)
            }
            put_i64(&mut out, end);
        }
    }
    Ok(out)
}

/// Constant answer per partition: error 0 + the current log end offset
/// (meta `len`, Fetch's high-watermark source); misses answer the
/// protocol defaults (error 3, end_offset -1).
fn epoch_answer(shared: &Shared, topic: &str, partition: i32) -> Result<(i16, i64), String> {
    if let Err(code) = mapping::validate_topic(topic.as_bytes()) {
        return Ok((code, -1));
    }
    let parent = topic.as_bytes();
    let prefix = hash::slot_with_prefix(parent).1;
    let Some(child) = mapping::partition_queue(&shared.store, &prefix, parent, partition)? else {
        return Ok((errors::UNKNOWN_TOPIC_OR_PARTITION, -1));
    };
    let mut stream = parent.to_vec();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    let end = mapping::latest_ordinal(&shared.store, &prefix, &stream)?.unwrap_or(0) as i64;
    Ok((errors::NONE, end))
}
