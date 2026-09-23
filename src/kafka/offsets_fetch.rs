//! OffsetFetch (api 9) v0-v7 -- the read side of the committed-offset
//! ledger, split out of `offsets_commit` (pure move: mounted as a child
//! module there and re-exported, so `offsets_commit::` paths are
//! unchanged). Schema/behavior notes live in `offsets_commit`'s header.

use crate::hash;
use crate::kafka::errors;
use crate::kafka::frame::{
    put_array_len, put_empty_tagged_fields, put_i16, put_i32, put_i64, put_nullable_string,
    put_string, Reader,
};
use crate::kafka::mapping;
use crate::kafka::{catalog, ledger};
use crate::state::Shared;

/// One partition of an OffsetFetch request.
struct FetchTarget {
    partition: i32,
}

/// Per-topic response sections: (topic, [(partition, committed
/// offset, error code)]).
type TopicSections = Vec<(Vec<u8>, Vec<(i32, i64, i16)>)>;

/// Handle OffsetFetch v0-v7 (v6+ is flexible: compact framing, tagged
/// tails; v7 ends the request with require_stable).
pub fn handle_offset_fetch(
    body: &mut Reader<'_>,
    version: i16,
    shared: &Shared,
) -> Result<Vec<u8>, String> {
    let bad = || "malformed offsetfetch request".to_string();
    let flex = version >= 6;
    // Request: group id, then either a null topics array (fetch all) or
    // a list of (topic, partitions).
    let group = if flex {
        body.compact_string().ok_or_else(bad)?
    } else {
        body.string().ok_or_else(bad)?
    };
    let counted = (if flex {
        body.compact_array_len()
    } else {
        body.array_len()
    })
    .ok_or_else(bad)?;
    let topics: Option<Vec<(String, Vec<i32>)>> = match counted {
        None => None,
        Some(n) => {
            let mut list = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let name = if flex {
                    body.compact_string().ok_or_else(bad)?
                } else {
                    body.string().ok_or_else(bad)?
                };
                let np = if flex {
                    body.compact_array_len()
                } else {
                    body.array_len()
                }
                .ok_or_else(bad)?
                .unwrap_or(0);
                let mut parts = Vec::with_capacity(np.min(4096));
                for _ in 0..np {
                    parts.push(body.i32().ok_or_else(bad)?);
                }
                if flex {
                    body.skip_tagged_fields().ok_or_else(bad)?; // topic struct tags
                }
                list.push((name, parts));
            }
            Some(list)
        }
    };
    if flex {
        if version >= 7 {
            body.boolean().ok_or_else(bad)?; // require_stable: no epoch fencing
        }
        body.skip_tagged_fields().ok_or_else(bad)?; // message tags
    }

    // Response. Classic v0/v1: topics only. v2 adds the trailing
    // top-level error code, v3 the leading throttle_time_ms.
    let mut out = Vec::new();
    if version >= 3 {
        put_i32(&mut out, 0); // throttle_time_ms
    }
    match &topics {
        // Null topics array: every commit of this group, grouped into
        // per-topic response sections.
        None => {
            let mut rows = ledger::scan_group(&shared.store, group.as_bytes())?;
            rows.sort_by(|a, b| a.stream.cmp(&b.stream));
            let mut sections: TopicSections = Vec::new();
            for r in rows {
                let Some((topic, partition, _child)) = split_stream(&r.stream) else {
                    continue;
                };
                match sections.last_mut() {
                    Some((t, parts)) if *t == topic => {
                        parts.push((partition, r.committed_ordinal as i64, errors::NONE))
                    }
                    _ => sections.push((
                        topic,
                        vec![(partition, r.committed_ordinal as i64, errors::NONE)],
                    )),
                }
            }
            put_sections(&mut out, version, flex, &sections);
        }
        Some(list) => {
            let mut sections: TopicSections = Vec::with_capacity(list.len());
            for (name, parts) in list {
                let mut rows = Vec::with_capacity(parts.len());
                for p in parts {
                    let t = FetchTarget { partition: *p };
                    let (committed, err) = fetch_one(shared, &group, name, &t)?;
                    rows.push((t.partition, committed, err));
                }
                sections.push((name.as_bytes().to_vec(), rows));
            }
            put_sections(&mut out, version, flex, &sections);
        }
    }
    if version >= 2 {
        put_i16(&mut out, errors::NONE); // top-level error code
    }
    if flex {
        put_empty_tagged_fields(&mut out); // message tags
    }
    Ok(out)
}

/// Encode the per-topic sections array (classic or compact framed).
fn put_sections(out: &mut Vec<u8>, version: i16, flex: bool, sections: &TopicSections) {
    if flex {
        crate::kafka::frame::put_compact_array_len(out, sections.len());
    } else {
        put_array_len(out, sections.len());
    }
    for (topic, parts) in sections {
        let name = String::from_utf8_lossy(topic).into_owned();
        if flex {
            crate::kafka::frame::put_compact_string(out, &name);
            crate::kafka::frame::put_compact_array_len(out, parts.len());
        } else {
            put_string(out, &name);
            put_array_len(out, parts.len());
        }
        for (partition, committed, err) in parts {
            emit_partition(out, version, flex, *partition, *committed, *err);
        }
        if flex {
            put_empty_tagged_fields(out); // topic struct tags
        }
    }
}

/// Committed offset of one (topic, partition, group); `-1` + error 3
/// for an unknown topic/partition, `-1` + NONE when simply never
/// committed (the Kafka distinction).
fn fetch_one(
    shared: &Shared,
    group: &str,
    topic: &str,
    t: &FetchTarget,
) -> Result<(i64, i16), String> {
    if mapping::validate_topic(topic.as_bytes()).is_err() {
        return Ok((-1, errors::INVALID_TOPIC_EXCEPTION));
    }
    let parent = topic.as_bytes();
    let prefix = hash::slot_with_prefix(parent).1;
    let Some(child) = mapping::partition_queue(&shared.store, &prefix, parent, t.partition)? else {
        return Ok((-1, errors::UNKNOWN_TOPIC_OR_PARTITION));
    };
    let mut stream = parent.to_vec();
    stream.push(b'/');
    stream.extend_from_slice(&child);
    Ok((
        ledger::load(&shared.store, &prefix, &stream, group.as_bytes())?
            .map_or(-1, |r| r.committed_ordinal as i64),
        errors::NONE,
    ))
}

/// `(topic, partition, child)` of a full stream name; `None` when the
/// child is not a kafka partition (`p<N>`/`q<N>`).
fn split_stream(stream: &[u8]) -> Option<(Vec<u8>, i32, Vec<u8>)> {
    let i = stream.iter().position(|&b| b == b'/')?;
    let (topic, child) = (stream[..i].to_vec(), stream[i + 1..].to_vec());
    let partition = catalog::partition_index(&child)?;
    Some((topic, partition, child))
}

/// One partition entry of the OffsetFetch response: partition,
/// committed_offset (-1 = none), [v6+ committed_leader_epoch -1],
/// metadata (always null: P2 stores none), error_code.
fn emit_partition(
    out: &mut Vec<u8>,
    version: i16,
    flex: bool,
    partition: i32,
    committed: i64,
    err: i16,
) {
    put_i32(out, partition);
    put_i64(out, committed);
    if version >= 5 {
        put_i32(out, -1); // committed_leader_epoch
    }
    if flex {
        crate::kafka::frame::put_compact_nullable_string(out, None);
    } else {
        put_nullable_string(out, None);
    }
    put_i16(out, err);
    if flex {
        put_empty_tagged_fields(out);
    }
}
