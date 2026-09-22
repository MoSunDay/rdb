//! ListOffsets (api 2) v0-v1: offset lookups by special timestamp or
//! wall-clock time over the mapped Lite queue.
//!
//! Version policy: v0-v1 advertised (v1 is the librdkafka workhorse;
//! the flexible v2+ shapes grow per-version fields, deferred until a
//! real SDK needs them). Requests: replica_id, topics[name[partition,
//! timestamp]]. Timestamps: -1 latest, -2 earliest, -3 max_timestamp
//! (answered as latest: arrival ids keep no per-record max), anything
//! else = by-timestamp lookup.
//!
//! Response v0 carries an int64 ARRAY per partition; v1 carries
//! (timestamp, offset) scalars -- both per the protocol schema include
//! an error_code. Not-found by-timestamp (past the log end) answers
//! offset -1 / timestamp -1, the broker behavior.

use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{put_array_len, put_i16, put_i32, put_i64, put_string, Reader};
use crate::kafka::mapping;
use crate::state::Shared;

/// One requested partition lookup.
struct OffsetQuery {
    partition: i32,
    timestamp: i64,
}

/// Resolved answer for one partition.
struct OffsetOut {
    error: i16,
    /// -1 = not found / errored.
    offset: i64,
    /// Echoed timestamp: -1 for latest/earliest, else the found entry's ms.
    timestamp: i64,
}

/// Handle ListOffsets v0/v1.
pub fn handle_list_offsets(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    let _replica_id = body.i32().ok_or("malformed listoffsets request")?;
    let n_topics = body
        .array_len()
        .ok_or("malformed listoffsets request")?
        .unwrap_or(0);
    let mut out = Vec::new();
    put_array_len(&mut out, n_topics);
    for _ in 0..n_topics {
        let name = body.string().ok_or("malformed listoffsets topic")?;
        let n_parts = body
            .array_len()
            .ok_or("malformed listoffsets request")?
            .unwrap_or(0);
        put_string(&mut out, &name);
        put_array_len(&mut out, n_parts);
        for _ in 0..n_parts {
            let q = OffsetQuery {
                partition: body.i32().ok_or("malformed listoffsets partition")?,
                timestamp: body.i64().ok_or("malformed listoffsets timestamp")?,
            };
            let r = resolve(shared, &name, &q)?;
            emit_partition(&mut out, version, q.partition, &r);
        }
    }
    Ok(out)
}

/// Resolve one query against the Lite store; `Err` = storage failure,
/// every Kafka-level miss is an in-band error code.
fn resolve(shared: &Shared, topic: &str, q: &OffsetQuery) -> Result<OffsetOut, String> {
    let miss = |code: i16| OffsetOut { error: code, offset: -1, timestamp: -1 };
    if let Err(code) = mapping::validate_topic(topic.as_bytes()) {
        return Ok(miss(code));
    }
    let parent = topic.as_bytes();
    let prefix = hash::slot_with_prefix(parent).1;
    let Some(child) = mapping::partition_queue(&shared.store, &prefix, parent, q.partition)?
    else {
        return Ok(miss(errors::UNKNOWN_TOPIC_OR_PARTITION));
    };
    let mut stream = parent.to_vec();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    match q.timestamp {
        -2 => Ok(OffsetOut { error: errors::NONE, offset: 0, timestamp: -1 }),
        -1 | -3 => {
            let len = mapping::latest_ordinal(&shared.store, &prefix, &stream)?
                .ok_or_else(|| "stream vanished".to_string())?;
            Ok(OffsetOut { error: errors::NONE, offset: len as i64, timestamp: -1 })
        }
        ts => {
            // By-timestamp: first entry with arrival ms >= ts (negative
            // probes clamp to the log start).
            match mapping::offset_by_timestamp(&shared.store, &prefix, &stream, ts.max(0) as u64)? {
                Some((ordinal, id)) => Ok(OffsetOut {
                    error: errors::NONE,
                    offset: ordinal as i64,
                    timestamp: id.ms as i64,
                }),
                None => Ok(miss(errors::NONE)), // past the end: offset -1, ts -1
            }
        }
    }
}

/// Encode one partition entry of the v0/v1 response body.
fn emit_partition(out: &mut Vec<u8>, version: i16, partition: i32, r: &OffsetOut) {
    put_i32(out, partition);
    put_i16(out, r.error);
    if version == 0 {
        if r.error == errors::NONE && r.offset >= 0 {
            put_array_len(out, 1);
            put_i64(out, r.offset);
        } else {
            put_array_len(out, 0);
        }
    } else {
        put_i64(out, r.timestamp);
        put_i64(out, r.offset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state;
    use crate::store::ops;
    use rocksdb::WriteBatch;

    use crate::lite::model;

    fn seed(sh: &state::Shared, topic: &str, ids: &[(u64, u64)]) {
        let parent = topic.as_bytes();
        let prefix = hash::slot_with_prefix(parent).1;
        let stream = format!("{topic}/p0").into_bytes();
        let mut meta = model::MetaPayload {
            created_ms: 1,
            len: ids.len() as u64,
            ..Default::default()
        };
        if let Some(&(ms, seq)) = ids.last() {
            meta.last_ms = ms;
            meta.last_seq = seq;
        }
        let mut wb = WriteBatch::default();
        wb.put(model::meta_key(&prefix, &stream), model::encode_meta_at(&meta, 0));
        for &(ms, seq) in ids {
            wb.put(
                model::entry_key(&prefix, &stream, model::EntryId { ms, seq }),
                model::encode_entry(&[(b"f", b"v")]),
            );
        }
        ops::batch_write(&sh.store, wb).unwrap();
    }

    fn req(topic: &str, parts: &[(i32, i64)]) -> Vec<u8> {
        let mut out = Vec::new();
        put_i32(&mut out, -1); // replica_id (client)
        put_array_len(&mut out, 1);
        put_string(&mut out, topic);
        put_array_len(&mut out, parts.len());
        for (p, ts) in parts {
            put_i32(&mut out, *p);
            put_i64(&mut out, *ts);
        }
        out
    }

    #[test]
    fn v1_latest_earliest_by_ts_and_misses() {
        let sh = state::testutil::shared_with(state::testutil::test_config());
        seed(&sh, "tt", &[(100, 0), (100, 1), (200, 0)]);
        let body = handle_list_offsets(
            &mut Reader::new(&req(
                "tt",
                &[(0, -1), (0, -2), (0, -3), (0, 100), (0, 150), (0, 9999), (7, -1)],
            )),
            1,
            &sh,
        )
        .unwrap();
        let mut r = Reader::new(&body);
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.string(), Some("tt".into()));
        assert_eq!(r.array_len(), Some(Some(7)));
        let got: Vec<(i32, i16, i64, i64)> = (0..7)
            .map(|_| {
                (
                    r.i32().unwrap(),
                    r.i16().unwrap(),
                    r.i64().unwrap(),
                    r.i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(got[0], (0, 0, -1, 3), "latest = len");
        assert_eq!(got[1], (0, 0, -1, 0), "earliest = 0");
        assert_eq!(got[2], (0, 0, -1, 3), "max_timestamp answers latest");
        assert_eq!(got[3], (0, 0, 100, 0), "exact ts hit");
        assert_eq!(got[4], (0, 0, 200, 2), "between lands on the next entry");
        assert_eq!(got[5], (0, 0, -1, -1), "past the end: not found");
        assert_eq!(got[6], (7, 3, -1, -1), "unknown partition");
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn v0_offsets_array_and_errors() {
        let sh = state::testutil::shared_with(state::testutil::test_config());
        seed(&sh, "tv", &[(10, 0)]);
        let body = handle_list_offsets(
            &mut Reader::new(&req("tv", &[(0, -1), (0, 999), (1, -1), (5, -1)])),
            0,
            &sh,
        )
        .unwrap();
        let mut r = Reader::new(&body);
        assert_eq!(r.array_len(), Some(Some(1)));
        r.string();
        assert_eq!(r.array_len(), Some(Some(4)));
        assert_eq!(r.i32(), Some(0));
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.i64(), Some(1));
        assert_eq!(r.i32(), Some(0));
        assert_eq!(r.i16(), Some(0));
        assert_eq!(r.array_len(), Some(Some(0)), "v0 miss = empty offsets");
        assert_eq!(r.i32(), Some(1));
        assert_eq!(r.i16(), Some(3), "partition without a queue");
        assert_eq!(r.array_len(), Some(Some(0)));
        assert_eq!(r.i32(), Some(5));
        assert_eq!(r.i16(), Some(3), "second unknown partition");
        assert_eq!(r.array_len(), Some(Some(0)));
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn invalid_topic_name_is_error_17() {
        let sh = state::testutil::shared_with(state::testutil::test_config());
        let body = handle_list_offsets(
            &mut Reader::new(&req("bad name", &[(0, -1)])),
            1,
            &sh,
        )
        .unwrap();
        let mut r = Reader::new(&body);
        assert_eq!(r.array_len(), Some(Some(1)));
        r.string();
        assert_eq!(r.array_len(), Some(Some(1)));
        assert_eq!(r.i32(), Some(0));
        assert_eq!(r.i16(), Some(17), "INVALID_TOPIC_EXCEPTION");
        assert_eq!(r.i64(), Some(-1));
        assert_eq!(r.i64(), Some(-1));
    }
}
