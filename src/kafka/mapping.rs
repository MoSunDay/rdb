//! Static topic<->queue mapping + the offset (entry-ordinal) conversions
//! the Kafka front needs.
//!
//! Naming: Kafka topic = Lite parent; partition N maps to the child queue
//! `p<N>` (the front's own canonical name) or, when only Lite auto-pick
//! queues exist, `q<N>` -- so a RESP-born topic is directly producible
//! over Kafka without migration. `p<N>` wins when both exist.
//!
//! Offsets are ORDINALS in the active entry set (0-based; latest = meta
//! `len`), not the physical `<ms>-<seq>` ids. Every conversion is a
//! read-only ordered prefix scan of the stream's entry window -- entry
//! ids sort lexicographically in key space, so counting needs only the
//! 16-byte key suffixes, never a decode of entry values. No functions
//! are added to `lite::read` (near its size cap); these walk the store
//! directly through `store::ops::for_each_from`.

use crate::lite::model::{self, EntryId};
use crate::lite::select;
use crate::store::{ops, Store};

use super::catalog;
use super::errors;

/// Child queue name the kafka front canonically uses for a partition.
pub fn topic_child(partition: i32) -> String {
    format!("p{partition}")
}

/// Full Lite stream name `parent/p<N>`.
pub fn stream_name(parent: &[u8], partition: i32) -> Vec<u8> {
    let mut out = parent.to_vec();
    out.push(b'/');
    out.extend_from_slice(topic_child(partition).as_bytes());
    out
}

/// Kafka topic-name rule (the Lite part rule): `[A-Za-z0-9._-]{1,64}`.
/// Anything else is error 17 INVALID_TOPIC_EXCEPTION.
pub fn validate_topic(name: &[u8]) -> Result<(), i16> {
    if crate::lite::valid_part(name) {
        Ok(())
    } else {
        Err(errors::INVALID_TOPIC_EXCEPTION)
    }
}

/// The child queue backing partition `partition` of `parent`: `p<N>` if
/// present, else `q<N>`, else `None` (unknown partition -- this front
/// never auto-creates partitions, matching allow_auto_topic_creation=
/// false).
pub fn partition_queue(
    store: &Store,
    prefix: &[u8],
    parent: &[u8],
    partition: i32,
) -> Result<Option<Vec<u8>>, String> {
    let children = select::discover_children(store, prefix, parent, catalog::QUEUE_LIMIT)?;
    for tag in [b'p', b'q'] {
        let want = format!("{}{}", tag as char, partition);
        if children.iter().any(|c| c.as_slice() == want.as_bytes()) {
            return Ok(Some(want.into_bytes()));
        }
    }
    Ok(None)
}

/// Latest offset = the stream meta `len`; `None` = no live stream.
pub fn latest_ordinal(store: &Store, prefix: &[u8], stream: &[u8]) -> Result<Option<u64>, String> {
    Ok(model::read_meta(store, prefix, stream, None)?.live().map(|m| m.len))
}

/// Entry id at `ordinal` (0-based); `None` when `ordinal >= len`.
pub fn ordinal_to_id(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    ordinal: u64,
) -> Result<Option<EntryId>, String> {
    let mut seen = 0u64;
    let mut hit = None;
    walk_ids(store, prefix, stream, &mut |id| {
        if seen == ordinal {
            hit = Some(id);
            return false;
        }
        seen += 1;
        true
    })?;
    Ok(hit)
}

/// Ordinal of the first entry with id >= `id` (== the number of entries
/// strictly before `id`); an id past the end maps to `len`.
pub fn id_to_ordinal(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    id: EntryId,
) -> Result<u64, String> {
    let mut count = 0u64;
    walk_ids(store, prefix, stream, &mut |e| {
        if e < id {
            count += 1;
            true
        } else {
            false
        }
    })?;
    Ok(count)
}

/// Ordinal + id of the first entry whose `ms` is >= `ts` (ListOffsets
/// by-timestamp); `None` when every entry is older.
pub fn offset_by_timestamp(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    ts: u64,
) -> Result<Option<(u64, EntryId)>, String> {
    let mut count = 0u64;
    let mut hit = None;
    walk_ids(store, prefix, stream, &mut |id| {
        if id.ms < ts {
            count += 1;
            true
        } else {
            hit = Some((count, id));
            false
        }
    })?;
    Ok(hit)
}

/// Ordered walk of the stream's entry ids (key suffixes only); `f`
/// returns false to stop.
fn walk_ids(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    f: &mut dyn FnMut(EntryId) -> bool,
) -> Result<(), String> {
    let base = model::entry_base(prefix, stream);
    let from = model::entry_key(prefix, stream, model::MIN_ID);
    ops::for_each_from(store, &from, false, &mut |k, _| {
        if !k.starts_with(&base) {
            return false; // left the stream's window
        }
        match id_from_key(&base, k) {
            Some(id) => f(id),
            None => true, // foreign key inside the window: skip
        }
    })
}

/// `<ms u64BE><seq u64BE>` suffix of an entry key (mirror of the
/// lite-internal `entries::id_from_key`).
fn id_from_key(base: &[u8], k: &[u8]) -> Option<EntryId> {
    let sfx = k.get(base.len()..)?;
    if sfx.len() != 16 {
        return None;
    }
    Some(EntryId {
        ms: u64::from_be_bytes(sfx[..8].try_into().ok()?),
        seq: u64::from_be_bytes(sfx[8..].try_into().ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash;
    use rocksdb::WriteBatch;

    /// In-process store (the `state` testutil fixture).
    fn shared() -> crate::state::Shared {
        crate::state::testutil::shared_with(crate::state::testutil::test_config())
    }

    /// Seed `parent/child` with `ids` (one dummy field pair each).
    fn seed(sh: &crate::state::Shared, parent: &str, child: &str, ids: &[EntryId]) {
        let mut stream = format!("{parent}/{child}").into_bytes();
        let prefix = hash::slot_with_prefix(parent.as_bytes()).1;
        let mut meta = model::MetaPayload {
            created_ms: 1,
            len: ids.len() as u64,
            ..Default::default()
        };
        if let Some(last) = ids.last() {
            meta.last_ms = last.ms;
            meta.last_seq = last.seq;
        }
        let mut batch = WriteBatch::default();
        batch.put(model::meta_key(&prefix, &stream), model::encode_meta_at(&meta, 0));
        for id in ids {
            batch.put(
                model::entry_key(&prefix, &stream, *id),
                model::encode_entry(&[(b"f", b"v")]),
            );
        }
        ops::batch_write(&sh.store, batch).unwrap();
    }

    fn ids(pairs: &[(u64, u64)]) -> Vec<EntryId> {
        pairs.iter().map(|&(ms, seq)| EntryId { ms, seq }).collect()
    }

    #[test]
    fn names_and_validation() {
        assert_eq!(topic_child(0), "p0");
        assert_eq!(topic_child(12), "p12");
        assert_eq!(stream_name(b"orders", 3), b"orders/p3".to_vec());
        assert_eq!(validate_topic(b"orders.v2"), Ok(()));
        assert_eq!(
            validate_topic(b"bad name"),
            Err(errors::INVALID_TOPIC_EXCEPTION)
        );
        assert_eq!(validate_topic(b""), Err(errors::INVALID_TOPIC_EXCEPTION));
        assert_eq!(validate_topic(b"x".repeat(65).as_slice()), Err(17));
    }

    #[test]
    fn partition_queue_prefers_p_over_q() {
        let sh = shared();
        let prefix = hash::slot_with_prefix(b"t").1;
        seed(&sh, "t", "q1", &ids(&[(5, 0)]));
        // Only q1 exists: partition 1 resolves through the fallback name.
        assert_eq!(
            partition_queue(&sh.store, &prefix, b"t", 1).unwrap(),
            Some(b"q1".to_vec())
        );
        assert_eq!(
            partition_queue(&sh.store, &prefix, b"t", 0).unwrap(),
            None,
            "q0 was never created"
        );
        seed(&sh, "t", "p1", &ids(&[(6, 0)]));
        assert_eq!(
            partition_queue(&sh.store, &prefix, b"t", 1).unwrap(),
            Some(b"p1".to_vec()),
            "canonical p<N> wins over q<N>"
        );
        assert_eq!(partition_queue(&sh.store, &prefix, b"t", 9).unwrap(), None);
    }

    #[test]
    fn ordinal_id_roundtrip_and_latest() {
        let sh = shared();
        // Same-ms seq run plus a later ms: the ordering trap for suffixes.
        let set = ids(&[(10, 0), (10, 1), (10, 2), (25, 0), (25, 1)]);
        seed(&sh, "t", "p0", &set);
        let prefix = hash::slot_with_prefix(b"t").1;
        let stream = b"t/p0".to_vec();
        assert_eq!(latest_ordinal(&sh.store, &prefix, &stream).unwrap(), Some(5));
        assert_eq!(
            latest_ordinal(&sh.store, &prefix, b"t/none").unwrap(),
            None
        );
        for (ordinal, want) in set.iter().enumerate() {
            let got = ordinal_to_id(&sh.store, &prefix, &stream, ordinal as u64)
                .unwrap()
                .expect("in range");
            assert_eq!(got, *want, "ordinal {ordinal}");
            assert_eq!(
                id_to_ordinal(&sh.store, &prefix, &stream, got).unwrap(),
                ordinal as u64,
                "roundtrip of {want:?}"
            );
        }
        // Past-the-end and boundary ordinals.
        assert_eq!(ordinal_to_id(&sh.store, &prefix, &stream, 5).unwrap(), None);
        assert_eq!(
            id_to_ordinal(&sh.store, &prefix, &stream, EntryId { ms: 10, seq: 3 }).unwrap(),
            3,
            "first id strictly greater counts the prefix"
        );
        assert_eq!(
            id_to_ordinal(&sh.store, &prefix, &stream, EntryId { ms: 0, seq: 0 }).unwrap(),
            0
        );
    }

    #[test]
    fn by_timestamp_boundaries() {
        let sh = shared();
        let set = ids(&[(10, 0), (10, 1), (25, 0)]);
        seed(&sh, "t", "p0", &set);
        let prefix = hash::slot_with_prefix(b"t").1;
        let stream = b"t/p0".to_vec();
        let find = |ts: u64| offset_by_timestamp(&sh.store, &prefix, &stream, ts).unwrap();
        assert_eq!(find(0), Some((0, set[0])), "ts before everything -> first");
        assert_eq!(find(10), Some((0, set[0])), "exact ms hit is inclusive");
        assert_eq!(find(11), Some((2, set[2])), "between ms lands on the next");
        assert_eq!(find(25), Some((2, set[2])));
        assert_eq!(find(26), None, "past the end: not found");
    }
}
