//! Committed-offset ledger (P2): one physical record per (partition
//! stream, consumer group) under its own kind family.
//!
//! Layout: `data_key(slot_prefix, KIND_STREAM_OFFSET, stream) ++ "/"
//! ++ group` -- the user key is the FULL partition stream name (e.g.
//! `t/p0`), so a partition delete reclaims its ledger rows through the
//! normal stream-reclaim path (`ds::expire::family_delete_entries`
//! folds the ledger window in; see the kind comment in `ds::codec`).
//!
//! Value: `encode_envelope(0, JSON)` with
//! `{"committed_ordinal","generation","leader"}`:
//! - `committed_ordinal` is stored EXACTLY as committed (the
//!   next-to-consume ordinal -- Fetch's offset semantics, no +/-1
//!   conversion). A commit past the log end is accepted and surfaces
//!   as OFFSET_OUT_OF_RANGE on the next Fetch.
//! - `generation` guards v1+ OffsetCommits: an incoming generation
//!   OLDER than the stored one answers ILLEGAL_GENERATION (no broker
//!   fencing beyond that in P2 -- the ledger never blocks a producer).
//! - `leader` is the last committing member_id (observability only).
//!
//! Ledger rows never make a key "exist" (not in `meta_kinds`), carry no
//! TTL (envelope 0), and are written through the fsync'd batch path.

use serde::{Deserialize, Serialize};

use crate::ds::codec::{self, KIND_STREAM_OFFSET};
use crate::store::{ops, Store};

/// Physical records examined by a whole-store group scan (bounds the
/// walk exactly like `catalog::list_topics`).
pub const GROUP_SCAN_LIMIT: usize = 100_000;

/// One ledger record: the decoded payload plus its identity.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LedgerRow {
    /// Full partition stream name (`t/p0`).
    #[serde(skip)]
    pub stream: Vec<u8>,
    /// Consumer group (the key suffix).
    #[serde(skip)]
    pub group: Vec<u8>,
    /// Slot prefix the row lives under -- the PARENT-derived slot (the
    /// same one the stream's own records use), so a stream purge (which
    /// folds the ledger window in through `family_delete_entries`)
    /// reclaims the rows.
    #[serde(skip)]
    pub prefix: Vec<u8>,
    pub committed_ordinal: u64,
    pub generation: i32,
    pub leader: String,
}

/// Physical ledger key of `(stream, group)`.
pub fn ledger_key(prefix: &[u8], stream: &[u8], group: &[u8]) -> Vec<u8> {
    let mut k = codec::data_key(prefix, KIND_STREAM_OFFSET, stream);
    k.push(b'/');
    k.extend_from_slice(group);
    k
}

/// `(stream, group)` of a physical ledger key; `None` for any other
/// key shape (wrong kind, missing `/group` suffix).
pub fn parse_key(pk: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let prefix_len = crate::ds::expire::slot_prefix_len(pk)?;
    let rest = &pk[prefix_len..];
    if rest.first() != Some(&KIND_STREAM_OFFSET) {
        return None;
    }
    let body = rest.get(1..)?;
    if body.len() < 4 {
        return None;
    }
    let slen = u32::from_be_bytes(body[..4].try_into().ok()?) as usize;
    let stream_end = 4usize.checked_add(slen)?;
    let stream = body.get(4..stream_end)?;
    let tail = body.get(4 + slen..)?;
    let split = tail.iter().position(|&b| b == b'/')?;
    let group = &tail[split + 1..];
    if group.is_empty() {
        return None;
    }
    Some((stream.to_vec(), group.to_vec()))
}

/// Read one row; `None` = no commit yet for that (stream, group).
pub fn load(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    group: &[u8],
) -> Result<Option<LedgerRow>, String> {
    let raw = ops::get_physical(store, &ledger_key(prefix, stream, group))?;
    Ok(raw.as_deref().and_then(decode_value).map(|(ordinal, gen, leader)| LedgerRow {
        stream: stream.to_vec(),
        group: group.to_vec(),
        prefix: prefix.to_vec(),
        committed_ordinal: ordinal,
        generation: gen,
        leader,
    }))
}

fn decode_value(raw: &[u8]) -> Option<(u64, i32, String)> {
    let (_, body) = codec::decode_envelope(raw);
    serde_json::from_slice::<Payload>(body)
        .ok()
        .map(|p| (p.committed_ordinal, p.generation, p.leader))
}

/// On-disk shape (serde needs the payload fields without the identity
/// skips, so this mirrors [`LedgerRow`] minus `#[serde(skip)]` noise).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Payload {
    committed_ordinal: u64,
    generation: i32,
    leader: String,
}

/// Put rows into `batch` (the caller owns the latch + the write). Each
/// row carries its own slot prefix (parent-derived).
pub fn put_rows(batch: &mut rocksdb::WriteBatch, rows: &[LedgerRow]) {
    for r in rows {
        let p = Payload {
            committed_ordinal: r.committed_ordinal,
            generation: r.generation,
            leader: r.leader.clone(),
        };
        let body = serde_json::to_vec(&p).unwrap_or_default();
        batch.put(
            ledger_key(&r.prefix, &r.stream, &r.group),
            codec::encode_envelope(0, &body),
        );
    }
}

/// Every row of `group` across ALL streams: one bounded ordered walk
/// from the start of the keyspace, matching kind-0x20 keys whose
/// suffix is `"/" ++ group`. O(store) like `catalog::list_topics`,
/// capped by [`GROUP_SCAN_LIMIT`] (beyond the cap rows are silently
/// absent, never a stall).
pub fn scan_group(store: &Store, group: &[u8]) -> Result<Vec<LedgerRow>, String> {
    let suffix = {
        let mut s = vec![b'/'];
        s.extend_from_slice(group);
        s
    };
    let mut out = Vec::new();
    let mut examined = 0usize;
    ops::for_each_from(store, b"", false, &mut |k, v| {
        examined += 1;
        if k.ends_with(&suffix) {
            if let Some((stream, g)) = parse_key(k) {
                if let Some((ordinal, gen, leader)) = decode_value(v) {
                    out.push(LedgerRow {
                        stream,
                        group: g,
                        prefix: Vec::new(), // not needed by readers
                        committed_ordinal: ordinal,
                        generation: gen,
                        leader,
                    });
                }
            }
        }
        examined < GROUP_SCAN_LIMIT
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Slot prefix of the stream name under test.
    fn prefix(stream: &[u8]) -> Vec<u8> {
        crate::hash::slot_with_prefix(stream).1
    }

    #[test]
    fn key_roundtrip_and_foreign_shapes() {
        let k = ledger_key(b"70/", b"t/p0", b"g1");
        assert_eq!(parse_key(&k), Some((b"t/p0".to_vec(), b"g1".to_vec())));
        // Wrong kind (a stream meta record) never parses.
        let meta = codec::data_key(b"70/", codec::KIND_STREAM_META, b"t/p0");
        assert_eq!(parse_key(&meta), None);
        // No group suffix.
        let bare = codec::data_key(b"70/", KIND_STREAM_OFFSET, b"t/p0");
        assert_eq!(parse_key(&bare), None);
        // Group containing '/' splits on the FIRST slash (the stream
        // length is explicit, so the split is unambiguous).
        let nested = ledger_key(b"70/", b"t/p0", b"region/1");
        assert_eq!(
            parse_key(&nested),
            Some((b"t/p0".to_vec(), b"region/1".to_vec()))
        );
        // stream containing '/' is the normal case.
        let s2 = ledger_key(b"70/", b"orders/eu/p1", b"g");
        assert_eq!(parse_key(&s2), Some((b"orders/eu/p1".to_vec(), b"g".to_vec())));
    }

    #[test]
    fn value_roundtrip() {
        let raw = {
            let p = Payload {
                committed_ordinal: 7,
                generation: 3,
                leader: "m-1".into(),
            };
            codec::encode_envelope(0, &serde_json::to_vec(&p).unwrap())
        };
        assert_eq!(decode_value(&raw), Some((7, 3, "m-1".to_string())));
        assert_eq!(decode_value(b"junk"), None);
    }

    #[test]
    fn load_and_scan_group() {
        let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
        let store = shared.store.clone();
        let pfx = prefix(b"t/p0");
        let mut batch = rocksdb::WriteBatch::default();
        put_rows(
            &mut batch,
            &[LedgerRow {
                stream: b"t/p0".to_vec(),
                group: b"g1".to_vec(),
                prefix: pfx.clone(),
                committed_ordinal: 5,
                generation: 2,
                leader: "m-1".into(),
            }],
        );
        ops::batch_write(&store, batch).unwrap();

        let row = load(&store, &pfx, b"t/p0", b"g1").unwrap().unwrap();
        assert_eq!(row.committed_ordinal, 5);
        assert_eq!(load(&store, &pfx, b"t/p0", b"other").unwrap(), None);

        let all = scan_group(&store, b"g1").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].stream, b"t/p0".to_vec());
        assert_eq!(all[0].committed_ordinal, 5);
        assert!(scan_group(&store, b"nogroup").unwrap().is_empty());
    }
}
