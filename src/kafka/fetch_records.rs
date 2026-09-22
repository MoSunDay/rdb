//! Record assembly for Fetch: walking a partition's Lite entries from
//! a requested ordinal and re-encoding them as one RecordBatch v2
//! (the inverse of `produce`'s field-pair writer).
//!
//! Byte budget: `budget` bounds the assembled records; the Kafka
//! at-least-one rule applies (a positive budget never yields zero
//! records while data exists). `WALK_CHUNK` entries are decoded per
//! store pass so a huge partition is walked incrementally.

use crate::kafka::record::{build_batch, BatchRecord};
use crate::lite::entries::scan_entries;
use crate::lite::model;
use crate::store::Store;

/// Entries decoded per store chunk while walking a partition.
pub(super) const WALK_CHUNK: usize = 256;

/// One entry flattened to its Kafka record shape (arrival ms + key/value).
struct RawRec {
    ms: u64,
    key: Option<Vec<u8>>,
    value: Option<Vec<u8>>,
}

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
                let size = 64 + e
                    .fields
                    .iter()
                    .map(|(f, v)| f.len() + v.len())
                    .sum::<usize>();
                if !recs.is_empty() && used + size > budget {
                    break 'walk;
                }
                used += size;
                let (key, value) = to_key_value(&e.fields);
                recs.push(RawRec { ms: e.id.ms, key, value });
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
            headers: Vec::new(),
        })
        .collect();
    Ok(build_batch(from_ordinal as i64, base_ms, &batch))
}

/// Inverse of `produce::record_fields`: entry pairs back to a Kafka
/// record's key/value.
///
/// - 0 pairs -> (None, None)
/// - 1 pair  -> ("__null__" = null-null tombstone) or value-only
/// - 2 pairs, no "h" pair -> (key, value) / (key, None) via the
///   `__null__` value marker
/// - anything longer or header-bearing -> generic envelope: value =
///   JSON `{"fields":[[name,hex(value)]...]}`, key None (the exact
///   bytes still round-trip; a plain Kafka client never sees this
///   shape because it can only produce the shapes above).
pub(super) fn to_key_value(fields: &[(Vec<u8>, Vec<u8>)]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    fn name(p: &(Vec<u8>, Vec<u8>)) -> &[u8] {
        p.0.as_slice()
    }
    match fields {
        [] => (None, None),
        [only] => {
            if name(only) == b"__null__" {
                (None, None)
            } else {
                (None, Some(only.1.clone()))
            }
        }
        [a, b] if name(a) != b"h" && name(b) != b"h" => {
            // Positional (the produce writer's 2-pair shapes are
            // [("k",key),("v",value)] and [("k",key),("__null__","")]).
            if name(b) == b"__null__" {
                (Some(a.1.clone()), None)
            } else {
                (Some(a.1.clone()), Some(b.1.clone()))
            }
        }
        _ => {
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
            (None, Some(json.into_bytes()))
        }
    }
}
