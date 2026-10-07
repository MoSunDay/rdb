//! Topic-admin trio (P3 backfill #1): CreateTopics (19) v0-v4,
//! DeleteTopics (20) v0-v3, CreatePartitions (37) v0-v1 -- classic
//! framing only (CreateTopics turns flexible at v5, DeleteTopics at
//! v4, CreatePartitions at v2 -- all above the caps; librdkafka's own
//! windows are 0-4 / 0-4 / 0-2, so it lands on v4 / v3 / v1).
//!
//! Single-node semantics (documented divergence): replication_factor
//! must be 1 or the -1 "unset" default (KIP-464), anything else is
//! INVALID_REPLICATION_FACTOR -- there is no second broker to place a
//! replica on; per-topic replica assignments are rejected outright
//! (INVALID_REPLICA_ASSIGNMENT) for the same reason; num_partitions -1
//! means the broker default: 1. Topic configs ride the request but are
//! parsed and ignored (no topic-config store; DescribeConfigs answers
//! defaults). The storage moves live in [`super::topic_store`].

use crate::kafka::catalog;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_i16, put_i32, put_nullable_string, put_string, Reader,
};
use crate::kafka::mapping;
use crate::kafka::topic_store;
use crate::state::Shared;

/// Partition-count cap per create/grow call: Kafka itself rejects
/// counts over 10_000 with INVALID_PARTITIONS, so this mirrors the
/// broker ceiling while bounding the latch+batch fan-out.
const MAX_PARTITIONS: i32 = 10_000;

/// One CreateTopics entry as decoded from the wire.
struct CreatableTopic {
    name: String,
    num_partitions: i32,
    replication_factor: i16,
    assignments: usize,
}

/// Skip one [broker_ids] array of a replica assignment.
fn skip_broker_ids(body: &mut Reader<'_>) -> Result<(), String> {
    for _ in 0..body.array_len().ok_or("malformed assignment")?.unwrap_or(0) {
        body.i32().ok_or("malformed assignment")?;
    }
    Ok(())
}

/// CreateTopics v0-v4. Response mirrors the request order; v2+ leads
/// with throttle_time_ms, v1+ carries a per-topic error_message.
pub async fn handle_create_topics(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed createtopics request".to_string();
    let n_topics = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut topics = Vec::with_capacity(n_topics.min(1024));
    for _ in 0..n_topics {
        let name = body.string().ok_or_else(bad)?;
        let num_partitions = body.i32().ok_or_else(bad)?;
        let replication_factor = body.i16().ok_or_else(bad)?;
        // Assignments: [partition_index, [broker_ids]]; counted only
        // (any non-empty set is rejected below).
        let assignments = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        for _ in 0..assignments {
            body.i32().ok_or_else(bad)?; // partition_index
            skip_broker_ids(body)?;
        }
        // Configs: [name, value] -- accepted and ignored (no
        // topic-config store; DescribeConfigs answers defaults).
        for _ in 0..body.array_len().ok_or_else(bad)?.unwrap_or(0) {
            body.string().ok_or_else(bad)?;
            body.string().ok_or_else(bad)?;
        }
        topics.push(CreatableTopic {
            name,
            num_partitions,
            replication_factor,
            assignments,
        });
    }
    body.i32().ok_or_else(bad)?; // timeout_ms
    let validate_only = version >= 1 && body.boolean().ok_or_else(bad)?;
    let mut out = Vec::new();
    if version >= 2 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_array_len(&mut out, topics.len());
    for t in &topics {
        let code = create_topic(shared, t, validate_only).await;
        put_string(&mut out, &t.name);
        put_i16(&mut out, code);
        if version >= 1 {
            put_nullable_string(&mut out, error_text(code));
        }
    }
    Ok(out)
}

/// Validate one CreatableTopic and (unless `validate_only`) create its
/// partition streams. Validation order: name, assignments, replication
/// factor, partition count, existence.
async fn create_topic(shared: &Shared, t: &CreatableTopic, validate_only: bool) -> i16 {
    if let Err(code) = mapping::validate_topic(t.name.as_bytes()) {
        return code;
    }
    if t.assignments > 0 {
        return errors::INVALID_REPLICA_ASSIGNMENT;
    }
    if t.replication_factor != 1 && t.replication_factor != -1 {
        return errors::INVALID_REPLICATION_FACTOR;
    }
    let parts = if t.num_partitions == -1 {
        topic_store::DEFAULT_NUM_PARTITIONS
    } else if !(1..=MAX_PARTITIONS).contains(&t.num_partitions) {
        return errors::INVALID_PARTITIONS;
    } else {
        t.num_partitions
    };
    let parent = t.name.as_bytes().to_vec();
    match topic_store::topic_exists(shared, &parent) {
        Ok(true) => return errors::TOPIC_ALREADY_EXISTS,
        Ok(false) => {}
        Err(_) => return errors::UNKNOWN_SERVER_ERROR,
    }
    if validate_only {
        return errors::NONE;
    }
    match topic_store::create_partitions(shared, &parent, 0, parts).await {
        Ok(()) => errors::NONE,
        Err(_) => errors::UNKNOWN_SERVER_ERROR,
    }
}

/// DeleteTopics v0-v3: [topic] in, (v1+ throttle +) [name, error] out,
/// request order preserved. Per topic the family delete runs in
/// [`super::topic_store::delete_topic`]; failures answer
/// UNKNOWN_SERVER_ERROR so the batch still makes progress.
pub async fn handle_delete_topics(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed deletetopics request".to_string();
    let n = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut names = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        names.push(body.string().ok_or_else(bad)?);
    }
    body.i32().ok_or_else(bad)?; // timeout_ms
    let mut out = Vec::new();
    if version >= 1 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    put_array_len(&mut out, names.len());
    for name in &names {
        let code = match topic_store::delete_topic(shared, name.as_bytes()).await {
            Ok(code) => code,
            Err(_) => errors::UNKNOWN_SERVER_ERROR,
        };
        put_string(&mut out, name);
        put_i16(&mut out, code);
    }
    Ok(out)
}

/// CreatePartitions v0-v1: grow the partition count. `count` is the
/// new TOTAL (Kafka semantics); a shrink answers INVALID_PARTITIONS.
pub async fn handle_create_partitions(
    body: &mut Reader<'_>,
    _version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    // v0/v1 share the request/response layout (throttle always first).
    let bad = || "malformed createpartitions request".to_string();
    let n = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut topics = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let name = body.string().ok_or_else(bad)?;
        let count = body.i32().ok_or_else(bad)?;
        // Assignments: [[broker_ids]] (nullable); non-empty is rejected.
        let assignments = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        for _ in 0..assignments {
            skip_broker_ids(body)?;
        }
        topics.push((name, count, assignments));
    }
    body.i32().ok_or_else(bad)?; // timeout_ms
    let validate_only = body.boolean().ok_or_else(bad)?;
    let mut out = Vec::new();
    put_i32(&mut out, 0); // throttle_time_ms (v0+)
    put_array_len(&mut out, topics.len());
    for (name, count, assignments) in &topics {
        let (code, msg) = grow_topic(shared, name, *count, *assignments, validate_only).await;
        put_string(&mut out, name);
        put_i16(&mut out, code);
        put_nullable_string(&mut out, msg);
    }
    Ok(out)
}

/// Validate and (unless `validate_only`) grow one topic to `count`
/// total partitions: fill every missing index in `0..count` through
/// the same create path CreateTopics uses.
async fn grow_topic(
    shared: &Shared,
    name: &str,
    count: i32,
    assignments: usize,
    validate_only: bool,
) -> (i16, Option<&'static str>) {
    if let Err(code) = mapping::validate_topic(name.as_bytes()) {
        return (code, error_text(code));
    }
    if assignments > 0 {
        return (
            errors::INVALID_REPLICA_ASSIGNMENT,
            error_text(errors::INVALID_REPLICA_ASSIGNMENT),
        );
    }
    if !(1..=MAX_PARTITIONS).contains(&count) {
        return (
            errors::INVALID_PARTITIONS,
            error_text(errors::INVALID_PARTITIONS),
        );
    }
    let parent = name.as_bytes().to_vec();
    let current = match catalog::partitions_of(&shared.store, &parent) {
        Ok(p) => p,
        Err(_) => return (errors::UNKNOWN_SERVER_ERROR, None),
    };
    if current.is_empty() {
        return (errors::UNKNOWN_TOPIC_OR_PARTITION, None);
    }
    // Partition counts only grow (Kafka: INVALID_PARTITIONS on shrink).
    if (count as usize) < current.len() {
        return (
            errors::INVALID_PARTITIONS,
            error_text(errors::INVALID_PARTITIONS),
        );
    }
    if validate_only {
        return (errors::NONE, None);
    }
    match topic_store::create_partitions(shared, &parent, 0, count).await {
        Ok(()) => (errors::NONE, None),
        Err(_) => (errors::UNKNOWN_SERVER_ERROR, None),
    }
}

/// Human-readable error_message for the topic-admin codes (null on
/// success, matching the broker's null-on-success replies).
fn error_text(code: i16) -> Option<&'static str> {
    match code {
        errors::NONE => None,
        errors::TOPIC_ALREADY_EXISTS => Some("topic already exists"),
        errors::INVALID_PARTITIONS => Some("partition count must only grow (1..=10000)"),
        errors::INVALID_REPLICATION_FACTOR => {
            Some("single-node broker: replication factor must be 1 or unset (-1)")
        }
        errors::INVALID_REPLICA_ASSIGNMENT => {
            Some("replica assignments are not supported on a single-node broker")
        }
        errors::INVALID_TOPIC_EXCEPTION => Some("invalid topic name"),
        errors::UNKNOWN_TOPIC_OR_PARTITION => Some("no such topic"),
        _ => Some(errors::error_name(code)),
    }
}
