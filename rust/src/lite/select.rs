//! Queue-selection strategies for `XPICK` / auto-pick `XADD <parent>`
//! (RocketMQ send-message queue selection). Pure functions; discovery is
//! a BOUNDED ordered scan of the parent's kind-0x0C window.
//!
//! `least_backlog` is approximate: it compares retained length among the
//! first `limit` discovered queues, not exact un-consumed counts.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ds::codec::{self, KIND_STREAM_META};
use crate::store::{ops, Store};

use super::model;

/// Physical keys examined per discovery call (bounds pathological topics).
pub const SCAN_LIMIT: usize = 5000;
/// Queues considered per pick (Lite topics carry a handful of queues).
pub const DEFAULT_LIMIT: usize = 16;
/// Queue created implicitly when a brand-new topic is picked.
pub const FIRST_QUEUE: &[u8] = b"q0";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    RoundRobin,
    Hash,
    LeastBacklog,
}

pub fn parse_strategy(s: &[u8]) -> Option<Strategy> {
    match s {
        b"round_robin" => Some(Strategy::RoundRobin),
        b"hash" => Some(Strategy::Hash),
        b"least_backlog" => Some(Strategy::LeastBacklog),
        _ => None,
    }
}

/// Bounded, ordered discovery of `parent`'s queues (child stream names).
pub fn discover_children(
    store: &Store,
    prefix: &[u8],
    parent: &[u8],
    limit: usize,
) -> Vec<Vec<u8>> {
    let mut pat = parent.to_vec();
    pat.push(b'/');
    let start = codec::data_key(prefix, KIND_STREAM_META, parent);
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut examined = 0usize;
    let _ = ops::for_each_from(store, &start, false, &mut |k, _| {
        examined += 1;
        // Left the kind-0x0C window of this slot: nothing more can match.
        if k.get(prefix.len()) != Some(&KIND_STREAM_META) {
            return false;
        }
        if let Some((kind, user, _)) = codec::decode_data_key(k, prefix.len()) {
            if kind == KIND_STREAM_META
                && user.starts_with(&pat)
                && user.len() > pat.len()
                && !user[pat.len()..].contains(&b'/')
            {
                out.push(user[pat.len()..].to_vec());
                if out.len() >= limit {
                    return false;
                }
            }
        }
        examined < SCAN_LIMIT
    });
    out
}

/// Per-parent round robin; `counters` lives in the Lite Runtime.
pub fn pick_round_robin(
    counters: &Mutex<HashMap<Vec<u8>, u64>>,
    parent: &[u8],
    children: &[Vec<u8>],
) -> Vec<u8> {
    if children.is_empty() {
        return FIRST_QUEUE.to_vec();
    }
    let mut map = counters.lock().unwrap();
    let c = map.entry(parent.to_vec()).or_insert(0);
    let pick = (*c % children.len() as u64) as usize;
    *c = c.wrapping_add(1);
    children[pick].clone()
}

/// Stable queue by shard key (CRC16 mod n) -- RocketMQ hash sharding.
pub fn pick_hash(children: &[Vec<u8>], shard: &[u8]) -> Option<Vec<u8>> {
    if children.is_empty() {
        return None;
    }
    let idx = crate::hash::crc16(shard) as usize % children.len();
    Some(children[idx].clone())
}

/// Smallest retained length wins; ties break by name. Missing metas are
/// skipped (never picked) so dead queues lose.
pub fn pick_least_backlog(
    store: &Store,
    prefix: &[u8],
    parent: &[u8],
    children: &[Vec<u8>],
) -> Vec<u8> {
    let mut best: Option<(u64, Vec<u8>)> = None;
    for child in children {
        let mut stream = parent.to_vec();
        stream.push(b'/');
        stream.extend_from_slice(child);
        let len = model::read_meta(store, prefix, &stream)
            .ok()
            .and_then(|r| r.live())
            .map(|m| m.len)
            .unwrap_or(u64::MAX);
        let better = best
            .as_ref()
            .is_none_or(|(bl, bc)| len < *bl || (len == *bl && child < bc));
        if better {
            best = Some((len, child.clone()));
        }
    }
    best.map(|(_, c)| c).unwrap_or_else(|| FIRST_QUEUE.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_parsing() {
        assert_eq!(parse_strategy(b"round_robin"), Some(Strategy::RoundRobin));
        assert_eq!(parse_strategy(b"hash"), Some(Strategy::Hash));
        assert_eq!(
            parse_strategy(b"least_backlog"),
            Some(Strategy::LeastBacklog)
        );
        assert_eq!(parse_strategy(b"sticky"), None);
    }

    #[test]
    fn round_robin_wraps_and_empty_defaults() {
        let counters = Mutex::new(HashMap::new());
        let kids: Vec<Vec<u8>> = vec![b"q0".to_vec(), b"q1".to_vec(), b"q2".to_vec()];
        let seq: Vec<Vec<u8>> = (0..7)
            .map(|_| pick_round_robin(&counters, b"t", &kids))
            .collect();
        let name = |v: &Vec<u8>| String::from_utf8_lossy(v).to_string();
        assert_eq!(
            seq.iter().map(name).collect::<Vec<_>>(),
            ["q0", "q1", "q2", "q0", "q1", "q2", "q0"]
        );
        assert_eq!(pick_round_robin(&counters, b"t", &[]), FIRST_QUEUE.to_vec());
    }

    #[test]
    fn hash_is_stable() {
        let kids: Vec<Vec<u8>> = vec![
            b"q0".to_vec(),
            b"q1".to_vec(),
            b"q2".to_vec(),
            b"q3".to_vec(),
        ];
        let a = pick_hash(&kids, b"user-42").unwrap();
        assert_eq!(a, pick_hash(&kids, b"user-42").unwrap());
        let idx = crate::hash::crc16(b"user-42") as usize % 4;
        assert_eq!(a, kids[idx]);
        assert!(pick_hash(&[], b"x").is_none());
    }
}
