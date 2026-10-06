//! Record assembly for Fetch: walking a partition's Lite entries from
//! a requested ordinal and re-encoding them as one RecordBatch v2
//! (the inverse of `produce`'s field-pair writer).
//!
//! Header replay: produce-shaped pairs (one "h" JSON pair beside at
//! most a "k" and a "v"/`__null__` slot) decode back to REAL record
//! headers; every other shape keeps the generic JSON envelope in
//! value plus a marker header so clients can detect the fallback
//! (plans/2026-10-06-mq-gap/03, scope one).
//!
//! Byte budget: `budget` bounds the assembled records; the Kafka
//! at-least-one rule applies (a positive budget never yields zero
//! records while data exists). `WALK_CHUNK` entries are decoded per
//! store pass so a huge partition is walked incrementally.

use crate::kafka::produce::parse_headers_json;
use crate::kafka::record::{build_batch, BatchRecord};
use crate::lite::entries::scan_entries;
use crate::lite::model;
use crate::store::Store;

/// Entries decoded per store chunk while walking a partition.
pub(super) const WALK_CHUNK: usize = 256;

/// Marker header (null value) replayed on envelope-fallback records:
/// the stored pairs were not a produce shape, so the exact bytes ride
/// in the value JSON instead of the key/value/header slots.
pub(super) const ENVELOPE_MARKER: &str = "rdb-envelope";

/// One entry flattened to its Kafka record shape (arrival ms +
/// key/value/headers).
struct RawRec {
    ms: u64,
    key: Option<Vec<u8>>,
    value: Option<Vec<u8>>,
    headers: Vec<(String, Option<Vec<u8>>)>,
}

/// Decoded record shape shared by the decode paths: key bytes, value
/// bytes and headers.
type DecodedFields = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Vec<(String, Option<Vec<u8>>)>,
);

/// Walk entries from `from_ordinal`, capped by a byte budget; at least
/// one record survives whenever the budget is positive (the Kafka
/// rule -- a cap never yields an empty-but-nonempty log read).
pub(super) fn collect_records(
    store: &Store,
    prefix: &[u8],
    stream: &[u8],
    from_ordinal: u64,
    budget: usize,
) -> Result<Vec<u8>, String> {
    let mut after = model::MIN_ID;
    let mut ordinal = 0u64;
    let mut used = 0usize;
    let mut recs: Vec<RawRec> = Vec::new();
    'walk: loop {
        let chunk = scan_entries(store, prefix, stream, after, WALK_CHUNK)?;
        if chunk.is_empty() {
            break;
        }
        let mut last = after;
        for e in &chunk {
            if ordinal >= from_ordinal {
                let (key, value, headers) = decode_fields(&e.fields);
                // Budget over the DECODED record (key + value + each
                // header's name/value + framing slack) so header
                // inflation cannot quietly blow past fetch max_bytes;
                // the envelope fallback's value already carries its
                // raw bytes, so it budgets itself.
                let size = 64
                    + key.as_deref().map_or(0, |k| k.len())
                    + value.as_deref().map_or(0, |v| v.len())
                    + headers
                        .iter()
                        .map(|(n, v)| n.len() + v.as_deref().map_or(0, |b| b.len()) + 8)
                        .sum::<usize>();
                if !recs.is_empty() && used + size > budget {
                    break 'walk;
                }
                used += size;
                recs.push(RawRec {
                    ms: e.id.ms,
                    key,
                    value,
                    headers,
                });
            }
            ordinal += 1;
            last = e.id;
        }
        if chunk.len() < WALK_CHUNK {
            break;
        }
        after = last;
    }
    // Batch identity: base_offset = the requested ordinal, first
    // timestamp = the first entry's arrival ms (deltas per entry).
    // No records -> NO batch bytes at all (a 0-length record set is
    // the broker's EOF shape, cheaper than shipping an empty batch).
    if recs.is_empty() {
        return Ok(Vec::new());
    }
    let base_ms = recs.first().map(|r| r.ms as i64).unwrap_or(0);
    let batch: Vec<BatchRecord<'_>> = recs
        .iter()
        .map(|r| BatchRecord {
            timestamp_delta: r.ms as i64 - base_ms,
            key: r.key.as_deref(),
            value: r.value.as_deref(),
            headers: r
                .headers
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_deref()))
                .collect(),
        })
        .collect();
    Ok(build_batch(from_ordinal as i64, base_ms, &batch))
}

/// Inverse of `produce::record_fields`: entry pairs back to a Kafka
/// record's key/value/headers.
///
/// - 0 pairs -> (None, None, no headers)
/// - 1 pair  -> ("__null__" = null-null tombstone) or value-only
/// - 2 pairs, no "h" pair -> (key, value) / (key, None) via the
///   `__null__` value marker
/// - header-bearing produce shapes -> REAL headers restored from the
///   "h" pair's JSON (see [`restore_headers`])
/// - anything else -> generic envelope: value = JSON
///   `{"fields":[[name,hex(value)]...]}`, key None, plus the
///   [`ENVELOPE_MARKER`] header (the exact bytes still round-trip; a
///   plain Kafka client never produces exotic shapes).
pub(super) fn decode_fields(fields: &[(Vec<u8>, Vec<u8>)]) -> DecodedFields {
    fn name(p: &(Vec<u8>, Vec<u8>)) -> &[u8] {
        p.0.as_slice()
    }
    match fields {
        [] => (None, None, Vec::new()),
        [only] if name(only) != b"h" => {
            if name(only) == b"__null__" {
                (None, None, Vec::new())
            } else {
                (None, Some(only.1.clone()), Vec::new())
            }
        }
        [a, b] if name(a) != b"h" && name(b) != b"h" => {
            // Positional (the produce writer's 2-pair shapes are
            // [("k",key),("v",value)] and [("k",key),("__null__","")]).
            if name(b) == b"__null__" {
                (Some(a.1.clone()), None, Vec::new())
            } else {
                (Some(a.1.clone()), Some(b.1.clone()), Vec::new())
            }
        }
        // Header-bearing candidate (or exotic storage): try the real
        // header restore first, keep the envelope as the fallback.
        _ => restore_headers(fields).unwrap_or_else(|| envelope_record(fields)),
    }
}

/// Produce-shaped restore: exactly one "h" pair whose bytes parse as
/// the headers JSON, every other pair (at most one each) named "k",
/// "v" or `__null__` -- the only shapes `record_fields` can write.
/// Anything else (broken JSON, unknown or duplicated names, more than
/// 3 pairs, "v" beside `__null__`) -> None = exotic fallback.
fn restore_headers(fields: &[(Vec<u8>, Vec<u8>)]) -> Option<DecodedFields> {
    if fields.len() > 3 {
        return None;
    }
    let mut key: Option<Vec<u8>> = None;
    let mut value: Option<Option<Vec<u8>>> = None; // outer None = no slot written
    let mut headers: Option<Vec<(String, Option<Vec<u8>>)>> = None;
    for (f, v) in fields {
        match f.as_slice() {
            b"h" if headers.is_none() => headers = Some(parse_headers_json(v)?),
            b"k" if key.is_none() => key = Some(v.clone()),
            b"v" if value.is_none() => value = Some(Some(v.clone())),
            b"__null__" if value.is_none() => value = Some(None),
            _ => return None,
        }
    }
    Some((key, value.unwrap_or(None), headers?))
}

/// Exotic fallback: value = JSON `{"fields":[[name,hex(value)]...]}`
/// (the exact bytes still round-trip), key None, one null-valued
/// [`ENVELOPE_MARKER`] header so a client can tell the fallback from a
/// produce-shaped record.
fn envelope_record(fields: &[(Vec<u8>, Vec<u8>)]) -> DecodedFields {
    let mut json = String::from("{\"fields\":[");
    for (i, (f, v)) in fields.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        json.push_str(&format!(
            "[{},\"{}\"]",
            serde_json::to_string(&String::from_utf8_lossy(f)).unwrap_or_default(),
            hex::encode(v)
        ));
    }
    json.push_str("]}");
    (
        None,
        Some(json.into_bytes()),
        vec![(ENVELOPE_MARKER.to_string(), None)],
    )
}
