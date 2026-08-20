//! Lite Mode data model: entry ids, binary payloads and the physical key
//! layout over the reserved STREAM kinds (0x0C-0x0F).
//!
//! One Lite stream ("parent/child" LiteTopic queue) is stored under the
//! slot prefix derived from its PARENT topic name, so every queue of a
//! topic lives in one contiguous window:
//!
//! ```text
//! meta   = data_key(prefix, 0x0C, stream)          value = envelope ++ json
//! entry  = data_key(prefix, 0x0D, stream) ++ <ms u64BE><seq u64BE>
//! group  = data_key(prefix, 0x0E, stream) ++ <group name>
//! ```
//!
//! Entry suffixes are fixed-width big-endian, so the natural key order IS
//! id order (RocketMQ ConsumeQueue equivalent). Entry payloads are a
//! compact field-value encoding, NOT the RESP frames.

use serde::{Deserialize, Serialize};

use crate::ds::codec::{
    self, KIND_STREAM_ENTRY, KIND_STREAM_GROUP, KIND_STREAM_META, STREAM_FAMILY,
};
use crate::ds::expire;
use crate::hash;
use crate::store::Store;

/// `<ms>-<seq>` stream entry id; ordering is (ms, seq) lexicographic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct EntryId {
    pub ms: u64,
    pub seq: u64,
}

pub const MIN_ID: EntryId = EntryId { ms: 0, seq: 0 };
pub const MAX_ID: EntryId = EntryId {
    ms: u64::MAX,
    seq: u64::MAX,
};

/// Parse `<ms>-<seq>` (both decimal u64).
pub fn parse_id(s: &[u8]) -> Option<EntryId> {
    let dash = s.iter().position(|&b| b == b'-')?;
    let ms = std::str::from_utf8(&s[..dash]).ok()?.parse::<u64>().ok()?;
    let seq = std::str::from_utf8(&s[dash + 1..])
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(EntryId { ms, seq })
}

/// Render `ms-seq`.
pub fn format_id(id: EntryId) -> String {
    format!("{}-{}", id.ms, id.seq)
}

/// 16-byte big-endian entry key suffix.
pub fn id_suffix(id: EntryId) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&id.ms.to_be_bytes());
    out.extend_from_slice(&id.seq.to_be_bytes());
    out
}

/// Auto id for `*`: current time, never decreasing. When the wall clock
/// went backwards (or repeats the last ms) the seq is bumped instead.
/// `None` when a strictly-greater id is impossible -- the last id sits at
/// `<ms, u64::MAX>` (or the `<u64::MAX, u64::MAX>` ceiling) and the clock
/// has not moved past its ms. Returning the saturated id would silently
/// EQUAL the last id, overwrite its entry and stall `last_id` (data
/// loss); the caller replies Redis's "exhausted the last possible ID".
pub fn auto_id(last: Option<EntryId>, now_ms: u64) -> Option<EntryId> {
    match last {
        Some(l) if now_ms <= l.ms && l.seq == u64::MAX => None,
        Some(l) if now_ms <= l.ms => Some(EntryId {
            ms: l.ms,
            seq: l.seq + 1,
        }),
        _ => Some(EntryId { ms: now_ms, seq: 0 }),
    }
}

/// One XRANGE bound: `-`/`+` map to MIN/MAX, a leading `(` means exclusive.
#[derive(Clone, Copy, Debug)]
pub struct RangeBound {
    pub id: EntryId,
    pub excl: bool,
}

pub fn parse_bound(s: &[u8]) -> Option<RangeBound> {
    match s {
        b"-" => Some(RangeBound {
            id: MIN_ID,
            excl: false,
        }),
        b"+" => Some(RangeBound {
            id: MAX_ID,
            excl: false,
        }),
        rest if rest.first() == Some(&b'(') => Some(RangeBound {
            id: parse_id(&rest[1..])?,
            excl: true,
        }),
        rest => Some(RangeBound {
            id: parse_id(rest)?,
            excl: false,
        }),
    }
}

/// Physical slot prefix of a stream: CRC16 slot of the PARENT topic name.
pub fn stream_prefix(stream: &[u8]) -> Option<Vec<u8>> {
    let slash = stream.iter().position(|&b| b == b'/')?;
    let (parent, child) = stream.split_at(slash);
    if parent.is_empty() || child.len() < 2 {
        return None; // no child part ("t/") or bare parent
    }
    Some(hash::slot_with_prefix(parent).1)
}

// ---- key constructors ----------------------------------------------------

pub fn meta_key(prefix: &[u8], stream: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_STREAM_META, stream)
}

/// Common prefix of every entry key of `stream` (iteration anchor).
pub fn entry_base(prefix: &[u8], stream: &[u8]) -> Vec<u8> {
    codec::data_key(prefix, KIND_STREAM_ENTRY, stream)
}

pub fn entry_key(prefix: &[u8], stream: &[u8], id: EntryId) -> Vec<u8> {
    codec::elem_key(prefix, KIND_STREAM_ENTRY, stream, &id_suffix(id))
}

pub fn group_key(prefix: &[u8], stream: &[u8], group: &[u8]) -> Vec<u8> {
    codec::elem_key(prefix, KIND_STREAM_GROUP, stream, group)
}

// ---- payloads -------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct MetaPayload {
    pub created_ms: u64,
    pub last_ms: u64,
    pub last_seq: u64,
    pub len: u64,
    /// Idle TTL configured via XIDLE, 0 = none.
    pub idle_ms: u64,
}

impl MetaPayload {
    pub fn last_id(&self) -> EntryId {
        EntryId {
            ms: self.last_ms,
            seq: self.last_seq,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct GroupPayload {
    pub created_ms: u64,
    /// Advisory delivery watermark (in-memory between flushes).
    pub delivered_ms: u64,
    pub delivered_seq: u64,
    /// Ack watermark -- the restart resume point (at-least-once).
    pub committed_ms: u64,
    pub committed_seq: u64,
}

fn encode_json<T: Serialize>(payload: &T) -> Vec<u8> {
    let body = serde_json::to_vec(payload).unwrap_or_default();
    codec::encode_envelope(0, &body)
}

fn decode_json<T: for<'de> Deserialize<'de>>(raw: &[u8]) -> Option<T> {
    let (_, body) = codec::decode_envelope(raw);
    serde_json::from_slice(body).ok()
}

/// Encode the meta record carrying `expire_ms` in the envelope (the lazy
/// purge check reads the envelope, the expire index feeds the active loop).
pub fn encode_meta_at(meta: &MetaPayload, expire_ms: u64) -> Vec<u8> {
    codec::encode_envelope(expire_ms, &serde_json::to_vec(meta).unwrap_or_default())
}

/// Deadline currently stored in the stream meta envelope (0 = none).
pub fn current_expire(store: &Store, prefix: &[u8], stream: &[u8]) -> u64 {
    crate::store::ops::get_physical(store, &meta_key(prefix, stream))
        .ok()
        .flatten()
        .map(|v| codec::decode_envelope(&v).0)
        .unwrap_or(0)
}

pub fn encode_meta(meta: &MetaPayload) -> Vec<u8> {
    encode_json(meta)
}

pub fn encode_group(group: &GroupPayload) -> Vec<u8> {
    encode_json(group)
}

pub fn decode_group(raw: &[u8]) -> Option<GroupPayload> {
    decode_json(raw)
}

/// `<count u32BE> ++ ( <flen u32BE> <f> <vlen u32BE> <v> )*`, enveloped.
pub fn encode_entry(pairs: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
    for (f, v) in pairs {
        body.extend_from_slice(&(f.len() as u32).to_be_bytes());
        body.extend_from_slice(f);
        body.extend_from_slice(&(v.len() as u32).to_be_bytes());
        body.extend_from_slice(v);
    }
    codec::encode_envelope(0, &body)
}

/// Decode a stored entry value -> field/value pairs.
pub fn decode_entry(raw: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    let (_, body) = codec::decode_envelope(raw);
    let mut i = 0usize;
    let take = |n: usize, i: &mut usize| -> Option<&[u8]> {
        let out = body.get(*i..*i + n)?;
        *i += n;
        Some(out)
    };
    let n = u32::from_be_bytes(take(4, &mut i)?.try_into().ok()?) as usize;
    // A count over the pairs the body can hold (8-byte header
    // each) is corrupt: reject instead of trusting it for
    // with_capacity (a forged u32::MAX would OOM).
    if n > body.len() / 8 {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let flen = u32::from_be_bytes(take(4, &mut i)?.try_into().ok()?) as usize;
        let f = take(flen, &mut i)?.to_vec();
        let vlen = u32::from_be_bytes(take(4, &mut i)?.try_into().ok()?) as usize;
        let v = take(vlen, &mut i)?.to_vec();
        out.push((f, v));
    }
    Some(out)
}

// ---- store reads (uniform lazy expiration) --------------------------------

/// Outcome of a meta read: `Purged` lets callers keep the live-stream
/// metric honest when the lazy TTL path fires.
#[derive(Debug, PartialEq)]
pub enum MetaRead {
    Missing,
    Purged,
    Live(MetaPayload),
}

impl MetaRead {
    pub fn live(self) -> Option<MetaPayload> {
        match self {
            MetaRead::Live(m) => Some(m),
            _ => None,
        }
    }
}

/// Read a stream's meta record; lazily purges an idle-expired stream.
pub fn read_meta(store: &Store, prefix: &[u8], stream: &[u8]) -> Result<MetaRead, String> {
    let raw = match crate::store::ops::get_physical(store, &meta_key(prefix, stream))? {
        None => return Ok(MetaRead::Missing),
        Some(v) => v,
    };
    let (expire_ms, body) = codec::decode_envelope(&raw);
    if expire::is_expired(expire_ms, expire::now_ms()) {
        expire::purge_if_expired(store, prefix, STREAM_FAMILY, stream, expire::now_ms());
        return Ok(MetaRead::Purged);
    }
    Ok(serde_json::from_slice(body)
        .ok()
        .map_or(MetaRead::Missing, MetaRead::Live))
}

/// Read one group record; lazily purges an idle-expired stream first.
pub fn read_group(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> Result<Option<GroupPayload>, String> {
    if read_meta(store, prefix, stream)?.live().is_none() {
        return Ok(None);
    }
    let raw = crate::store::ops::get_physical(store, &group_key(prefix, stream, group))?;
    Ok(raw.as_deref().and_then(decode_group))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_id_bumps_seq_moves_ms_and_errors_at_saturation() {
        let sat = EntryId {
            ms: 100,
            seq: u64::MAX,
        };
        // Same (or earlier) ms with a saturated seq: no strictly-greater
        // id exists -- None, never the last id again (which would
        // silently overwrite the entry and stall last_id).
        assert_eq!(auto_id(Some(sat), 100), None);
        assert_eq!(auto_id(Some(sat), 99), None);
        assert_eq!(
            auto_id(Some(MAX_ID), u64::MAX),
            None,
            "the absolute id ceiling is exhausted too"
        );
        // Same ms, seq below the ceiling: bump the seq.
        assert_eq!(
            auto_id(Some(EntryId { ms: 100, seq: 5 }), 100),
            Some(EntryId { ms: 100, seq: 6 })
        );
        // A later ms: restart at seq 0.
        assert_eq!(
            auto_id(Some(EntryId { ms: 100, seq: 7 }), 101),
            Some(EntryId { ms: 101, seq: 0 })
        );
        // Fresh stream (no last id): the clock position, seq 0.
        assert_eq!(auto_id(None, 42), Some(EntryId { ms: 42, seq: 0 }));
    }

    #[test]
    fn decode_entry_rejects_corrupt_huge_count() {
        // Only the count, zero pairs: a forged u32::MAX must be refused
        // before it reaches with_capacity.
        let raw = codec::encode_envelope(0, &u32::MAX.to_be_bytes());
        assert_eq!(decode_entry(&raw), None);
    }

    #[test]
    fn decode_entry_roundtrip_and_truncated() {
        let pairs = [
            (b"f1".as_slice(), b"v1".as_slice()),
            (b"f2".as_slice(), b"".as_slice()),
        ];
        assert_eq!(
            decode_entry(&encode_entry(&pairs)),
            Some(vec![
                (b"f1".to_vec(), b"v1".to_vec()),
                (b"f2".to_vec(), Vec::new()),
            ])
        );

        // Claims n=2 but carries only one complete pair: the guard lets
        // the honest count through and the take loop hits the wall.
        let one = encode_entry(&[(b"f1", b"v1")]);
        let (_, one_pair) = codec::decode_envelope(&one);
        let mut body = Vec::new();
        body.extend_from_slice(&2u32.to_be_bytes());
        body.extend_from_slice(&one_pair[4..]);
        assert_eq!(decode_entry(&codec::encode_envelope(0, &body)), None);
    }
}
