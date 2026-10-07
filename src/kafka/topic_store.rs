//! Storage orchestration for the topic-admin trio (P3 backfill #1):
//! the create/grow/delete primitives the wire handlers (and the
//! produce auto-create path) share.
//!
//! Creation follows the layout the front already serves: partition N
//! of topic T is the Lite stream `T/p<N>` under T's parent slot prefix
//! (`hash::slot_with_prefix`), i.e. the same physical shape an XADD to
//! `T/p<N>` produces -- catalog/mapping see the new partitions with no
//! migration.
//!
//! Deletion routes through the SAME family-delete path DEL and the
//! idle reap use (`command::keys_core::delete_records`, whose
//! `ds::expire::family_delete_entries` fold covers the stream family
//! 0x0C-0x0F, the committed-offset ledger 0x20 and the staged delay
//! rows 0x1D), plus the partition's nested DLQ streams (`T/pN/dlq` and
//! friends are full streams in the same slot). Deleting every physical
//! child of the topic name removes the topic from Metadata.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::keys_core;
use crate::ds::{expire, latch, wait};
use crate::hash;
use crate::kafka::catalog;
use crate::kafka::errors;
use crate::kafka::mapping;
use crate::lite::select;
use crate::lite::{model, offset};
use crate::state::Shared;
use crate::store::ops;

/// Broker default when a create request sends num_partitions = -1
/// (KIP-464): one partition, mirroring Kafka's num.partitions=1.
pub const DEFAULT_NUM_PARTITIONS: i32 = 1;

/// Does the topic own at least one partition stream (i.e. exists)?
pub fn topic_exists(shared: &Shared, parent: &[u8]) -> Result<bool, String> {
    Ok(!catalog::partitions_of(&shared.store, parent)?.is_empty())
}

/// Create the topic if missing (one default partition): the produce
/// auto-create entry point (#2). `Ok(())` also when it already exists;
/// `Err(code)` for an invalid name or storage failure.
pub async fn ensure_topic(shared: &Shared, parent: &[u8]) -> Result<(), i16> {
    mapping::validate_topic(parent)?;
    match topic_exists(shared, parent) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(_) => return Err(errors::UNKNOWN_SERVER_ERROR),
    }
    create_partitions(shared, parent, 0, DEFAULT_NUM_PARTITIONS)
        .await
        .map_err(|_| errors::UNKNOWN_SERVER_ERROR)
}

/// Create partition streams for every missing index in `lo..hi` under
/// `parent`'s slot prefix: one empty meta per stream (the same record
/// an XADD to `parent/pN` writes), one WriteBatch + fsync, every
/// target stream meta latched first (sorted, the deadlock convention).
pub async fn create_partitions(
    shared: &Shared,
    parent: &[u8],
    lo: i32,
    hi: i32,
) -> Result<(), String> {
    let prefix = hash::slot_with_prefix(parent).1;
    let mut streams = Vec::new();
    for idx in lo..hi {
        streams.push(mapping::stream_name(parent, idx));
    }
    let mut keys: Vec<Vec<u8>> = streams
        .iter()
        .map(|s| model::meta_key(&prefix, s))
        .collect();
    keys.sort();
    keys.dedup();
    let mut guards = Vec::with_capacity(keys.len());
    for k in &keys {
        guards.push(latch::lock(&shared.latch, k).await);
    }
    let now = expire::now_ms();
    let mut wb = WriteBatch::default();
    let mut fresh = Vec::new();
    for stream in &streams {
        if model::read_meta(&shared.store, &prefix, stream, None)?
            .live()
            .is_some()
        {
            continue; // already exists (e.g. CreatePartitions refill)
        }
        let meta = model::MetaPayload {
            created_ms: now,
            ..Default::default()
        };
        wb.put(
            model::meta_key(&prefix, stream),
            model::encode_meta_at(&meta, 0),
        );
        fresh.push(stream.clone());
    }
    ops::batch_write_async(Arc::clone(&shared.store), wb).await?;
    for stream in &fresh {
        // Same fresh-stream bookkeeping the produce path does.
        shared
            .lite
            .stats
            .streams_live
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        offset::remove_stream(&shared.lite.offsets, stream);
        wait::notify(&shared.wait_hub, &model::meta_key(&prefix, stream));
    }
    wait::notify(&shared.wait_hub, &model::meta_key(&prefix, parent));
    Ok(())
}

/// Delete one topic: every physical child stream of the parent name
/// (its partitions AND their nested DLQ streams) through the DEL
/// family-delete path. `Err` never escapes to the caller -- storage
/// failures are mapped so a topic batch makes progress.
pub async fn delete_topic(shared: &Shared, name: &[u8]) -> Result<i16, String> {
    if let Err(code) = mapping::validate_topic(name) {
        return Ok(code);
    }
    let parent = name.to_vec();
    let prefix = hash::slot_with_prefix(&parent).1;
    let children =
        select::discover_children(&shared.store, &prefix, &parent, catalog::QUEUE_LIMIT)?;
    if children.is_empty() {
        return Ok(errors::UNKNOWN_TOPIC_OR_PARTITION);
    }
    // Full delete set: each partition stream plus the nested children
    // it owns (the default DLQ is `<partition>/dlq`), byte-sorted so
    // latch acquisition is deadlock-safe.
    let mut targets: Vec<Vec<u8>> = Vec::new();
    for child in &children {
        let mut stream = parent.clone();
        stream.push(b'/');
        stream.extend_from_slice(child);
        targets.push(stream.clone());
        for nested in
            select::discover_children(&shared.store, &prefix, &stream, catalog::QUEUE_LIMIT)?
        {
            let mut dlq = stream.clone();
            dlq.push(b'/');
            dlq.extend_from_slice(&nested);
            targets.push(dlq);
        }
    }
    targets.sort();
    targets.dedup();
    for stream in &targets {
        delete_stream(shared, &prefix, stream).await?;
    }
    // Wake anyone parked on the topic name (fetch long-polls park on
    // child metas, which woke on their own delete notify above).
    wait::notify(&shared.wait_hub, &model::meta_key(&prefix, &parent));
    Ok(errors::NONE)
}

/// Delete one stream family through the SAME path DEL uses
/// (`keys_core::delete_records`: latch + `family_delete_entries`,
/// which folds the 0x20 ledger and 0x1D delay rows, + the cached-group
/// eviction), additionally under the lite stream latch the produce
/// path serializes on. `Ok(false)` = no such stream.
async fn delete_stream(shared: &Shared, prefix: &[u8], stream: &[u8]) -> Result<bool, String> {
    let _guard = latch::lock(&shared.latch, &model::meta_key(prefix, stream)).await;
    let gone = keys_core::delete_records(shared, prefix, stream, expire::now_ms()).await?;
    if gone {
        wait::notify(&shared.wait_hub, &model::meta_key(prefix, stream));
    }
    Ok(gone)
}
