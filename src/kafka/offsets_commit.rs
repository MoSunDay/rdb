//! OffsetCommit (api 8) v0-v2 + OffsetFetch (api 9) v0-v7: the consumer
//! side of the committed-offset ledger (`kafka::ledger`).
//!
//! Schema note (official protocol, NOT the task sketch): v1 is NOT
//! identical to v0 -- v1 adds generation_id + member_id (the JoinGroup
//! fencing pair) and v2 adds retention_time on top. v0 carries neither,
//! so a v0 commit PRESERVES the stored generation/leader (it cannot
//! state either) and skips the ILLEGAL_GENERATION check. retention_time
//! is parsed and ignored (rows carry no TTL).
//!
//! Fencing layers (P3 wired the first): (1) the COORDINATOR runtime --
//! an active group demands an enrolled member and the CURRENT
//! generation (zombie members/generations answer 25/22 before any
//! ledger work); (2) the LEDGER's stored generation -- for groups
//! absent from the runtime (restart wiped membership, or the group went
//! Empty) this is the degraded high-water check an older generation
//! still cannot pass. v0 carries neither field and skips both; nothing
//! blocks producers or reads.
//!
//! Committed offsets are stored AS GIVEN (next-to-consume ordinals; no
//! +/-1 conversion). A commit past the log end is accepted -- the next
//! Fetch reports OFFSET_OUT_OF_RANGE (the broker behavior).

use std::sync::Arc;

use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_i16, put_i32, put_string, Reader};
use crate::kafka::mapping;
use crate::kafka::{ledger, ledger::LedgerRow};
use crate::state::Shared;
use crate::store::ops;

/// One partition of an OffsetCommit request.
struct CommitTarget {
    partition: i32,
    offset: i64,
}
/// Handle OffsetCommit v0-v2. Always answers (acks-style empty replies
/// do not exist here).
pub async fn handle_offset_commit(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
    coord: &crate::kafka::coordinator::CoordRuntime,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed offsetcommit request".to_string();
    let group = body.string().ok_or_else(bad)?;
    // v1+ carries the fencing pair; v2 additionally retention_time.
    let (generation, member_id) = if version >= 1 {
        let g = body.i32().ok_or_else(bad)?;
        let m = body.string().ok_or_else(bad)?;
        (g, m)
    } else {
        (-1, String::new())
    };
    if version >= 2 {
        let _retention_ms = body.i64().ok_or_else(bad)?;
    }
    // Fence layer 1 (v1+): the coordinator runtime. A fenced request
    // answers the SAME error on every partition and writes nothing.
    let fence = if version >= 1 {
        crate::kafka::coordinator::commit_fence(coord, &group, generation, &member_id)
    } else {
        None
    };
    let n_topics = body.array_len().ok_or_else(bad)?.unwrap_or(0);
    let mut topics_out: Vec<(String, Vec<(i32, i16)>)> = Vec::with_capacity(n_topics.min(1024));
    let mut rows: Vec<LedgerRow> = Vec::new();
    // Distinct ledger keys this request may rewrite, sorted: guards are
    // acquired in one global order so two multi-partition commits never
    // deadlock against each other.
    let mut latch_keys: Vec<Vec<u8>> = Vec::new();
    for _ in 0..n_topics {
        let name = body.string().ok_or_else(bad)?;
        let n_parts = body.array_len().ok_or_else(bad)?.unwrap_or(0);
        let mut parts_out = Vec::with_capacity(n_parts.min(1024));
        for _ in 0..n_parts {
            let partition = body.i32().ok_or_else(bad)?;
            let offset = body.i64().ok_or_else(bad)?;
            let _metadata = body.nullable_string().ok_or_else(bad)?; // not stored
            let err = match fence {
                Some(code) => code,
                None => commit_one(
                    shared,
                    &group,
                    version,
                    generation,
                    &member_id,
                    &name,
                    CommitTarget { partition, offset },
                    &mut latch_keys,
                    &mut rows,
                ),
            };
            parts_out.push((partition, err));
        }
        topics_out.push((name, parts_out));
    }
    if !rows.is_empty() {
        let mut guards = Vec::new();
        for k in &latch_keys {
            guards.push(crate::ds::latch::lock(&shared.latch, k).await);
        }
        let mut batch = rocksdb::WriteBatch::default();
        ledger::put_rows(&mut batch, &rows);
        ops::batch_write_async(Arc::clone(&shared.store), batch)
            .await
            .map_err(|_| "offset commit write failed".to_string())?;
    }
    Ok(commit_body(&topics_out))
}

/// Validate + read-modify-write plan one target; the row (if any) is
/// appended to `rows` for the single batched write.
#[allow(clippy::too_many_arguments)]
fn commit_one(
    shared: &Shared,
    group: &str,
    version: i16,
    generation: i32,
    member_id: &str,
    topic: &str,
    t: CommitTarget,
    latch_keys: &mut Vec<Vec<u8>>,
    rows: &mut Vec<LedgerRow>,
) -> i16 {
    if mapping::validate_topic(topic.as_bytes()).is_err() {
        return errors::INVALID_TOPIC_EXCEPTION;
    }
    if t.offset < 0 {
        return errors::OFFSET_OUT_OF_RANGE;
    }
    let parent = topic.as_bytes();
    let prefix = hash::slot_with_prefix(parent).1;
    let Some(child) = mapping::partition_queue(&shared.store, &prefix, parent, t.partition)
        .unwrap_or(None)
    else {
        return errors::UNKNOWN_TOPIC_OR_PARTITION;
    };
    let mut stream = parent.to_vec();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    let existing = ledger::load(&shared.store, &prefix, &stream, group.as_bytes())
        .unwrap_or(None);
    if version >= 1 {
        if let Some(ref row) = existing {
            if row.generation > generation {
                return errors::ILLEGAL_GENERATION;
            }
        }
    }
    // v0 keeps the stored fencing fields; v1+ stamps the request's.
    let (gen, leader) = match (&existing, version >= 1) {
        (Some(row), false) => (row.generation, row.leader.clone()),
        _ => (generation, member_id.to_string()),
    };
    let key = ledger::ledger_key(&prefix, &stream, group.as_bytes());
    if !latch_keys.contains(&key) {
        latch_keys.push(key);
    }
    rows.push(LedgerRow {
        stream,
        group: group.as_bytes().to_vec(),
        prefix,
        committed_ordinal: t.offset as u64,
        generation: gen,
        leader,
    });
    errors::NONE
}

/// OffsetCommit response v0-v2 (identical shape): responses[topic,
/// partitions[partition, error_code]].
fn commit_body(topics: &[(String, Vec<(i32, i16)>)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_array_len(&mut out, topics.len());
    for (name, parts) in topics {
        put_string(&mut out, name);
        put_array_len(&mut out, parts.len());
        for (partition, err) in parts {
            put_i32(&mut out, *partition);
            put_i16(&mut out, *err);
        }
    }
    out
}

/// OffsetFetch (api 9) v0-v7 lives in `offsets_fetch.rs` (file-size
/// split, pure move); the re-export keeps every
/// `offsets_commit::handle_offset_fetch` path stable.
#[path = "offsets_fetch.rs"]
mod offsets_fetch;
pub use offsets_fetch::handle_offset_fetch;
