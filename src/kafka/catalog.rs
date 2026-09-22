//! Topic catalog: the topic/partition view of the Lite storage that the
//! Kafka Metadata response serves.
//!
//! Mapping: Kafka **topic** = Lite **parent**; Kafka **partition** = one
//! Lite child queue. Child names this front maps to a partition index:
//! `p<N>` (created by the kafka front itself from P1 on) and `q<N>`
//! (Lite's XADD auto-pick queues), so a topic born over RESP is visible
//! over Kafka with partition 0 without migration.
//!
//! Physical layout note: every queue of a parent lives under the parent's
//! slot prefix (`hash::slot_with_prefix`), so per-topic partition
//! discovery is one bounded ordered scan (reuses `lite::select`).

use crate::ds::codec::KIND_STREAM_META;
use crate::hash;
use crate::lite::select;
use crate::store::{ops, Store};

/// Physical keys examined by the all-topics scan (bounds the walk).
pub const TOPIC_SCAN_LIMIT: usize = 100_000;
/// Queues considered per topic partition listing.
pub const QUEUE_LIMIT: usize = 256;

/// All parents owning at least one queue: a bounded ordered walk of every
/// kind-0x0C record across all slots. The walk is O(store) -- it touches
/// non-stream keys too (they only fail the kind check) -- so it is capped
/// by [`TOPIC_SCAN_LIMIT`]; a database larger than the cap under-reports
/// topics instead of stalling the connection.
pub fn list_topics(store: &Store) -> Result<Vec<Vec<u8>>, String> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut examined = 0usize;
    ops::for_each_from(store, b"", false, &mut |k, _| {
        examined += 1;
        if let Some(user) = stream_meta_user(k) {
            if let Some(parent) = parent_of(user) {
                if !out.contains(&parent) {
                    out.push(parent);
                }
            }
        }
        examined < TOPIC_SCAN_LIMIT
    })?;
    Ok(out)
}

/// User key of a kind-0x0C physical key when it is one (any slot prefix:
/// the first `/` ends the decimal slot prefix; kind is the next byte).
fn stream_meta_user(k: &[u8]) -> Option<&[u8]> {
    let slash = k.iter().position(|&b| b == b'/')?;
    let body = &k[slash + 1..];
    if body.first() != Some(&KIND_STREAM_META) {
        return None;
    }
    let len = u32::from_be_bytes(body.get(1..5)?.try_into().ok()?) as usize;
    body.get(5..5 + len)
}

/// Parent part of a full `parent/child` stream name.
fn parent_of(user: &[u8]) -> Option<Vec<u8>> {
    let i = user.iter().position(|&b| b == b'/')?;
    Some(user[..i].to_vec())
}

/// Partition indexes of `parent`, ascending; empty = topic unknown.
/// Partition 0 is always reported for an existing topic (Kafka clients
/// cannot handle a zero-partition topic).
pub fn partitions_of(store: &Store, parent: &[u8]) -> Result<Vec<i32>, String> {
    if parent.is_empty() {
        return Ok(Vec::new());
    }
    let prefix = hash::slot_with_prefix(parent).1;
    let children = select::discover_children(store, &prefix, parent, QUEUE_LIMIT)?;
    let mut idx: Vec<i32> = children.iter().filter_map(|c| partition_index(c)).collect();
    idx.sort_unstable();
    idx.dedup();
    if !idx.is_empty() && !idx.contains(&0) {
        idx.insert(0, 0);
    }
    Ok(idx)
}

/// `p<N>` (kafka front) or `q<N>` (Lite auto-pick) -> `N`; anything else
/// (a manually named queue like "eu-west") is not a Kafka partition and
/// is skipped.
pub(crate) fn partition_index(child: &[u8]) -> Option<i32> {
    let first = *child.first()?;
    if first != b'p' && first != b'q' {
        return None;
    }
    let digits = &child[1..];
    if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_index_forms() {
        assert_eq!(partition_index(b"p0"), Some(0));
        assert_eq!(partition_index(b"q12"), Some(12));
        assert_eq!(partition_index(b"p"), None);
        assert_eq!(partition_index(b"rx0"), None);
        assert_eq!(partition_index(b"p1x"), None);
        assert_eq!(partition_index(b""), None);
        assert_eq!(partition_index(b"p999999999999"), None); // i32 overflow
    }

    #[test]
    fn stream_meta_user_key_shapes() {
        // <slot>/<kind><len u32><user>
        let mut k = b"354/".to_vec();
        k.push(KIND_STREAM_META);
        k.extend_from_slice(&3u32.to_be_bytes());
        k.extend_from_slice(b"t/c");
        assert_eq!(stream_meta_user(&k), Some(&b"t/c"[..]));
        // Wrong kind / truncated length.
        let mut bad = b"354/".to_vec();
        bad.push(crate::ds::codec::KIND_HASH_META);
        bad.extend_from_slice(&3u32.to_be_bytes());
        bad.extend_from_slice(b"t/c");
        assert_eq!(stream_meta_user(&bad), None);
        assert_eq!(stream_meta_user(b"354/\x0c\x00\x00"), None);
        assert_eq!(parent_of(b"orders/q0"), Some(b"orders".to_vec()));
        assert_eq!(parent_of(b"orders"), None);
    }
}
