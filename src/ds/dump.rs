//! DUMP/RESTORE wire format for MIGRATE (slot-migration transport).
//!
//! Format (all integers big-endian):
//! ```text
//! [0]            version (1)
//! [1]            kind byte: 0x00 raw string, else the meta kind
//! [2..6]         record count: u32
//! records:       body_len u32 | body | value_len u32 | value   (repeated)
//! ```
//! `body` is the physical key WITHOUT the slot prefix: the bare user key
//! for raw strings, `kind|len|user_key[|elem suffix]` for typed records.
//! The expire-index entry (0xFD) is NOT transported: RESTORE rebuilds it
//! from the TTL argument (Redis RESTORE semantics) or from the embedded
//! envelope deadline when the TTL argument is 0.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::command::keys_core;
use crate::ds::{codec, expire};
use crate::store::{key_upper_bound, ops, Store};

/// Current dump version byte.
pub const DUMP_VERSION: u8 = 1;

/// One physical record of a dumped key (key body after the slot prefix).
#[derive(Debug, PartialEq)]
pub struct Record {
    pub body: Vec<u8>,
    pub value: Vec<u8>,
}

/// Serialize `key` (of `prefix`) for transport; `None` when absent.
/// Lazily purges a due record first, exactly like every read path.
pub fn dump_key(store: &Arc<Store>, prefix: &[u8], key: &[u8], now: u64) -> Option<Vec<u8>> {
    let state = keys_core::resolve_arc(store, prefix, key, now);
    let (kind, records) = match state {
        keys_core::KeyState::Missing => return None,
        keys_core::KeyState::RawString { value } => (
            codec::KIND_STRING,
            vec![Record {
                body: key.to_vec(),
                value,
            }],
        ),
        keys_core::KeyState::Enveloped { kind, payload, .. } => {
            // The meta record plus every element record of THIS key, via
            // one exact per-kind window: `data_key` sorts the kind byte
            // FIRST, so a span across kinds would swallow other keys'
            // records that sort between the meta and element kinds.
            let family = codec::family_of(kind).expect("enveloped kind has a family");
            let mut records = Vec::new();
            for k in family.0..=family.1 {
                let lower = codec::data_key(prefix, k, key);
                let upper = key_upper_bound(&lower).unwrap_or_default();
                ops::for_each_from(store, &lower, false, &mut |pk, v| {
                    if pk >= upper.as_slice() {
                        return false; // left this (kind, key) window
                    }
                    records.push(Record {
                        body: pk[prefix.len()..].to_vec(),
                        value: v.to_vec(),
                    });
                    true
                })
                .ok()?;
            }
            // Stream dumps carry the Kafka committed-offset ledger rows
            // too (they belong to the partition stream being migrated);
            // the ledger kind sits in its own family, so its window is
            // appended explicitly.
            if family == codec::STREAM_FAMILY {
                let lower = codec::data_key(prefix, codec::KIND_STREAM_OFFSET, key);
                let upper = key_upper_bound(&lower).unwrap_or_default();
                ops::for_each_from(store, &lower, false, &mut |pk, v| {
                    if pk >= upper.as_slice() {
                        return false; // left this (kind, key) window
                    }
                    records.push(Record {
                        body: pk[prefix.len()..].to_vec(),
                        value: v.to_vec(),
                    });
                    true
                })
                .ok()?;
            }
            if records.is_empty() {
                // Envelope read raced a purge: ship the resolved payload.
                records.push(Record {
                    body: codec::data_key(b"", kind, key),
                    value: codec::encode_envelope(0, &payload),
                });
            }
            (kind, records)
        }
    };
    let mut out = Vec::with_capacity(6 + records.len() * 8);
    out.push(DUMP_VERSION);
    out.push(kind);
    out.extend_from_slice(&(records.len() as u32).to_be_bytes());
    for r in &records {
        out.extend_from_slice(&(r.body.len() as u32).to_be_bytes());
        out.extend_from_slice(&r.body);
        out.extend_from_slice(&(r.value.len() as u32).to_be_bytes());
        out.extend_from_slice(&r.value);
    }
    Some(out)
}

fn bad_payload() -> String {
    "ERR DUMP payload version or checksum are wrong".to_string()
}

/// Split `rest` into `(first n bytes, remainder)`.
fn take(rest: &[u8], n: usize) -> Option<(&[u8], &[u8])> {
    if rest.len() < n {
        return None;
    }
    Some(rest.split_at(n))
}

fn be32(rest: &[u8]) -> Option<(usize, &[u8])> {
    let (b, r) = take(rest, 4)?;
    Some((u32::from_be_bytes(b.try_into().ok()?) as usize, r))
}

/// Decode a dump payload back into `(kind, records)`.
pub fn decode_dump(data: &[u8]) -> Result<(u8, Vec<Record>), String> {
    let (first, rest) = take(data, 1).ok_or_else(bad_payload)?;
    let version = first[0];
    if version != DUMP_VERSION {
        return Err(bad_payload());
    }
    let (first, rest) = take(rest, 1).ok_or_else(bad_payload)?;
    let kind = first[0];
    let (count, mut rest) = be32(rest).ok_or_else(bad_payload)?;
    let mut records = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        let (blen, r) = be32(rest).ok_or_else(bad_payload)?;
        rest = r;
        let (body, r) = take(rest, blen).ok_or_else(bad_payload)?;
        rest = r;
        let (vlen, r) = be32(rest).ok_or_else(bad_payload)?;
        rest = r;
        let (value, r) = take(rest, vlen).ok_or_else(bad_payload)?;
        rest = r;
        records.push(Record {
            body: body.to_vec(),
            value: value.to_vec(),
        });
    }
    Ok((kind, records))
}

/// Write a dumped key into `batch` (target side). `ttl` is the absolute
/// expiry in ms (0 = keep the embedded deadline); the expire-index entry
/// is rebuilt from the effective deadline so the sweeper sees the key.
/// Record bodies embed the SOURCE key, so every record is re-rooted at
/// `key` (raw strings and typed children included). Returns the meta kind
/// written.
pub fn restore_key(
    batch: &mut WriteBatch,
    prefix: &[u8],
    key: &[u8],
    data: &[u8],
    ttl: u64,
) -> Result<u8, String> {
    let (kind, records) = decode_dump(data)?;
    if kind == codec::KIND_STRING {
        // Raw string: exactly one record, always the root.
        let r = records.first().ok_or_else(bad_payload)?;
        if ttl > 0 {
            // Raw string gains a TTL: switch to the STRING_TTL shape.
            let root = codec::data_key(prefix, codec::KIND_STRING_TTL, key);
            batch.put(&root, codec::encode_envelope(ttl, &r.value));
            expire::set_ttl_entries(batch, prefix, root, 0, ttl);
        } else {
            batch.put(physical(prefix, key), &r.value);
        }
        return Ok(kind);
    }
    // Typed records: body = kind|orig_len|orig_key[|elem_suffix] -- the
    // physical key WITHOUT the slot prefix, so every record's own kind byte
    // re-roots it at the right layout (meta records and element records use
    // different kinds). The first record must be the meta root (no suffix):
    // it carries the envelope and gets the TTL/expire-index treatment.
    let fam = codec::family_of(kind).ok_or_else(bad_payload)?;
    for (i, r) in records.iter().enumerate() {
        let (k, rest) = take(&r.body, 1).ok_or_else(bad_payload)?;
        let (olen, rest) = be32(rest).ok_or_else(bad_payload)?;
        // Stream dumps also carry Kafka ledger rows: their kind lives in
        // OFFSET_FAMILY (a deliberate outlier -- see KIND_STREAM_OFFSET),
        // accepted here for stream restores.
        let ledger_row = fam == codec::STREAM_FAMILY && k[0] == codec::KIND_STREAM_OFFSET;
        if (i == 0 && k[0] != kind)
            || (codec::family_of(k[0]) != Some(fam) && !ledger_row)
            || rest.len() < olen
        {
            return Err(bad_payload());
        }
        let suffix = &rest[olen..];
        // Re-root at `key`: the body embeds the SOURCE key, and element
        // records carry their OWN kind byte (meta kinds and element kinds
        // differ -- e.g. hash fields live under KIND_HASH_FLD).
        let mut pk = Vec::with_capacity(prefix.len() + 5 + key.len() + suffix.len());
        pk.extend_from_slice(prefix);
        pk.push(k[0]);
        pk.extend_from_slice(&(key.len() as u32).to_be_bytes());
        pk.extend_from_slice(key);
        pk.extend_from_slice(suffix);
        if suffix.is_empty() {
            let (embedded, payload) = codec::decode_envelope(&r.value);
            let deadline = if ttl > 0 { ttl } else { embedded };
            batch.put(&pk, codec::encode_envelope(deadline, payload));
            if deadline > 0 {
                expire::set_ttl_entries(batch, prefix, pk, 0, deadline);
            }
        } else {
            batch.put(&pk, &r.value);
        }
    }
    Ok(kind)
}

/// Physical key for a bare (raw-string) record.
fn physical(prefix: &[u8], key: &[u8]) -> Vec<u8> {
    let mut pk = Vec::with_capacity(prefix.len() + key.len());
    pk.extend_from_slice(prefix);
    pk.extend_from_slice(key);
    pk
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testutil;

    // The prefix for slot 70 (kept as a real slot so the test exercises
    // the physical key layout).
    fn prefix70() -> &'static [u8] {
        b"70/"
    }

    #[test]
    fn dump_restore_roundtrip_raw_string() {
        let (_guard, shared) = {
            let guard = crate::command::string::TEST_STORE_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (guard, testutil::shared_with(testutil::test_config()))
        };
        // Seed a raw string directly under the slot-70 prefix (bypassing
        // the hash so the physical key is deterministic).
        {
            let mut out = Vec::new();
            let argv = vec![b"k".to_vec(), b"v".to_vec()];
            let mut ctx = crate::command::test_ctx(&shared, prefix70().to_vec(), argv, &mut out);
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(crate::command::string::set(&mut ctx));
            assert_eq!(out, b"+OK\r\n");
        }
        let data = dump_key(&shared.store, prefix70(), b"k", 0).expect("dumped");
        assert_eq!(data[0], DUMP_VERSION);
        assert_eq!(data[1], codec::KIND_STRING);
        // restore into a fresh batch
        let mut batch = WriteBatch::default();
        restore_key(&mut batch, prefix70(), b"k2", &data, 0).unwrap();
        // apply + read back
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(crate::store::ops::batch_write_async(
                Arc::clone(&shared.store),
                batch,
            ))
            .unwrap();
        let got = crate::store::ops::get_physical(&shared.store, b"70/k2").unwrap();
        assert_eq!(got, Some(b"v".to_vec()));
    }

    #[test]
    fn dump_restore_roundtrip_typed_reroots_children() {
        let (_guard, shared) = {
            let guard = crate::command::string::TEST_STORE_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (guard, testutil::shared_with(testutil::test_config()))
        };
        // Seed a hash under the slot-70 prefix: meta + two field records
        // (the multi-record payload is what MIGRATE ships for real hashes).
        {
            let mut out = Vec::new();
            let argv = vec![
                b"h".to_vec(),
                b"f1".to_vec(),
                b"v1".to_vec(),
                b"f2".to_vec(),
                b"v2".to_vec(),
            ];
            let mut ctx = crate::command::test_ctx(&shared, prefix70().to_vec(), argv, &mut out);
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(crate::command::hash_cmd::hset(&mut ctx));
            assert_eq!(out, b":2\r\n");
        }
        let data = dump_key(&shared.store, prefix70(), b"h", 0).expect("dumped");
        assert_eq!(data[1], codec::KIND_HASH_META);
        let (_, records) = decode_dump(&data).unwrap();
        assert_eq!(records.len(), 3, "meta + 2 fields, got {records:?}");
        // Restore under a DIFFERENT key name (different length too, so
        // child re-rooting is exercised).
        let mut batch = WriteBatch::default();
        restore_key(&mut batch, prefix70(), b"h2", &data, 0).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(crate::store::ops::batch_write_async(
                Arc::clone(&shared.store),
                batch,
            ))
            .unwrap();
        // The re-restored key dumps to a payload whose bodies re-root at
        // h2; the original h is untouched.
        let again = dump_key(&shared.store, prefix70(), b"h2", 0).expect("redumped");
        let (kind, records) = decode_dump(&again).unwrap();
        assert_eq!(kind, codec::KIND_HASH_META);
        assert_eq!(records.len(), 3, "re-dump carries meta + fields");
        assert!(
            records.iter().all(|r| r.body[5..7] == *b"h2"),
            "bodies: {records:?}"
        );
        // The restored hash is READABLE through the exact hgetall path
        // (`collect_fields`) -- the e2e regression this guards against.
        let page =
            crate::ds::hash_ds::collect_fields(&shared.store, prefix70(), b"h2", None, None, 0)
                .expect("read back");
        let flat: Vec<&[u8]> = page
            .fields
            .iter()
            .flat_map(|(f, v)| [f.as_slice(), v.as_slice()])
            .collect();
        assert_eq!(
            flat,
            vec![b"f1".as_slice(), b"v1", b"f2", b"v2"],
            "restored fields"
        );
        let (kind, _) = decode_dump(&data).unwrap();
        assert_eq!(kind, codec::KIND_HASH_META);
        assert!(
            dump_key(&shared.store, prefix70(), b"h", 0).is_some(),
            "source intact"
        );
    }
}
