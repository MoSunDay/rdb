//! PEL (Pending Entries List): per-group "delivered but not yet acked"
//! records persisted over the reserved KIND_STREAM_PEND (0x0F) window.
//!
//! Key layout inside one stream's kind-0x0F window (the whole window is
//! reclaimed with the stream by the STREAM_FAMILY purge ranges):
//!
//! ```text
//! pend entry = data_key(prefix, 0x0F, stream) ++ <group> ++ 0x00 ++ <id 16B BE>
//! consumer   = data_key(prefix, 0x0F, stream) ++ <group> ++ 0x01 ++ <name>
//! ```
//!
//! PEL keys are id-ordered (fixed-width BE suffix), so XPENDING range
//! queries and XAUTOCLAIM cursor walks are natural forward scans. The
//! consumer registry (tag 0x01) sorts after every PEL key of the group
//! (tag 0x00), keeping the two sub-spaces disjoint and prefix-scannable.
//!
//! Durability: every PEL mutation (delivery / claim / ack / consumer
//! admin) rides one synchronous latched `ctx.commit` batch, the same
//! WAL path entries already use -- a kill -9 loses at most the
//! in-flight batch, and the group watermark rewind (delivered ->
//! committed on restart) redelivers anything not acked: at-least-once.

use rocksdb::WriteBatch;
use serde::{Deserialize, Serialize};

use crate::ds::codec::{self, KIND_STREAM_PEND};
use crate::store::{ops, Store};

use super::model::{self, EntryId};

/// Consumer identity in the runtime registry: (stream, group, consumer),
/// all raw bytes.
pub type ConsumerId = (Vec<u8>, Vec<u8>, Vec<u8>);

/// Tag byte separating PEL rows (id-ordered) from consumer-registry keys.
pub const PEND_TAG: u8 = 0x00;
/// Registry keys sort strictly after every PEL row of the same group.
pub const CONSUMER_TAG: u8 = 0x01;

/// One pending record: who holds it, since when, how many times handed out.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PendState {
    pub consumer: Vec<u8>,
    pub delivered_ms: u64,
    pub times_delivered: u64,
}

/// Consumer-registry record (XGROUP CREATECONSUMER / first delivery).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ConsumerState {
    pub created_ms: u64,
}

/// A PEL row as scanned from disk.
pub struct PendRow {
    pub id: EntryId,
    pub state: PendState,
}

/// A consumer-registry row as scanned from disk.
pub struct ConsumerRow {
    pub name: Vec<u8>,
    pub created_ms: u64,
}

// ---- key constructors ----------------------------------------------------

/// Common prefix of ALL kind-0x0F keys of one group (both tags).
pub fn pend_base(prefix: &[u8], stream: &[u8], group: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_STREAM_PEND, stream, group)
}

/// Iteration anchor of the group's PEL rows (id-ordered sub-space).
pub fn pend_entry_base(prefix: &[u8], stream: &[u8], group: &[u8]) -> Vec<u8> {
    let mut base = pend_base(prefix, stream, group);
    base.push(PEND_TAG);
    base
}

/// Physical key of one pending record.
pub fn pend_key(prefix: &[u8], stream: &[u8], group: &[u8], id: EntryId) -> Vec<u8> {
    let mut key = pend_entry_base(prefix, stream, group);
    key.extend_from_slice(&model::id_suffix(id));
    key
}

/// Physical key of one consumer-registry record.
pub fn consumer_key(prefix: &[u8], stream: &[u8], group: &[u8], consumer: &[u8]) -> Vec<u8> {
    let mut key = pend_base(prefix, stream, group);
    key.push(CONSUMER_TAG);
    key.extend_from_slice(consumer);
    key
}

/// Exclusive end of the group's whole kind-0x0F window: the base with its
/// last byte carry-incremented (falling back to the next kind's base when
/// the base is all 0xFF). Used with `WriteBatch::delete_range` so XGROUP
/// DESTROY does not need to enumerate keys.
pub fn pend_range_end(prefix: &[u8], stream: &[u8], group: &[u8]) -> Vec<u8> {
    let base = pend_base(prefix, stream, group);
    for i in (0..base.len()).rev() {
        if base[i] != 0xFF {
            let mut end = base.clone();
            end[i] += 1;
            end.truncate(i + 1);
            return end;
        }
    }
    // All 0xFF (pathological group name): wipe to the next kind's window.
    codec::data_key(prefix, crate::ds::codec::KIND_JSON, stream)
}

/// Range delete of one group's whole PEL window (entries + consumers).
pub fn delete_group_pend(batch: &mut WriteBatch, prefix: &[u8], stream: &[u8], group: &[u8]) {
    batch.delete_range(
        pend_base(prefix, stream, group),
        pend_range_end(prefix, stream, group),
    );
}

// ---- payloads ------------------------------------------------------------

fn encode<T: Serialize>(payload: &T) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    codec::encode_envelope(0, &body)
}

fn decode<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> Option<T> {
    let (_, body) = codec::decode_envelope(raw);
    serde_json::from_slice(body).ok()
}

pub fn encode_pend(state: &PendState) -> Vec<u8> {
    encode(state)
}

pub fn decode_pend(raw: &[u8]) -> Option<PendState> {
    decode(raw)
}

pub fn encode_consumer(state: &ConsumerState) -> Vec<u8> {
    encode(state)
}

pub fn decode_consumer(raw: &[u8]) -> Option<ConsumerState> {
    decode(raw)
}

// ---- scans ---------------------------------------------------------------

/// PEL rows of the group with id >= `from` (inclusive), id order; `limit`
/// caps the returned rows (None = unbounded).
pub fn scan_pend(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
    from: EntryId,
    limit: Option<usize>,
) -> Result<Vec<PendRow>, String> {
    let base = pend_entry_base(prefix, stream, group);
    let from_key = pend_key(prefix, stream, group, from);
    let mut out = Vec::new();
    ops::for_each_from(store, &from_key, false, &mut |k, v| {
        if !k.starts_with(&base) {
            return false; // left the group's PEL window
        }
        let suffix = &k[base.len()..];
        if suffix.len() == 16 && k.len() >= base.len() {
            let ms = u64::from_be_bytes(suffix[..8].try_into().unwrap_or([0; 8]));
            let seq = u64::from_be_bytes(suffix[8..].try_into().unwrap_or([0; 8]));
            if let Some(state) = decode_pend(v) {
                out.push(PendRow {
                    id: EntryId { ms, seq },
                    state,
                });
            }
        }
        limit.is_none_or(|cap| out.len() < cap)
    })?;
    Ok(out)
}

/// Point read of one pending record.
pub fn get_pend(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
    id: EntryId,
) -> Result<Option<PendState>, String> {
    ops::get_physical(store, &pend_key(prefix, stream, group, id))
        .map(|v| v.and_then(|raw| decode_pend(&raw)))
}

/// Consumer-registry rows of the group, ordered by name.
pub fn scan_consumers(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> Result<Vec<ConsumerRow>, String> {
    let base = {
        let mut b = pend_base(prefix, stream, group);
        b.push(CONSUMER_TAG);
        b
    };
    let mut out = Vec::new();
    ops::for_each_from(store, &base, false, &mut |k, v| {
        if !k.starts_with(&base) {
            return false;
        }
        let name = k[base.len()..].to_vec();
        if let Some(state) = decode_consumer(v) {
            out.push(ConsumerRow {
                name,
                created_ms: state.created_ms,
            });
        }
        true
    })?;
    Ok(out)
}

/// Exact pending-row count of the group (backlog reload after restart).
pub fn count_pend(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> Result<u64, String> {
    let base = pend_entry_base(prefix, stream, group);
    let mut n: u64 = 0;
    ops::for_each_from(store, &base, false, &mut |k, _| {
        if !k.starts_with(&base) {
            return false;
        }
        if k.len() == base.len() + 16 {
            n += 1;
        }
        true
    })?;
    Ok(n)
}
