//! M3 distributed SQL writes: cross-slot two-phase commit (2PC).
//!
//! ## Why
//! SQL rows live in slot-prefixed physical keys; a cluster node owns
//! only its slot band. Any SQL write whose rows span more than the
//! local band must land on several nodes atomically, or a crash
//! mid-batch leaves half a transaction behind. When the cluster is NOT
//! ready (or every participant is this node) the write takes the exact
//! M1/M2 single-batch local path -- 2PC never touches single-node
//! deployments.
//!
//! ## Protocol (see [`proto`])
//! One dedicated TCP listener per node (`sql_rpc_bind`, empty =
//! disabled), u32-BE length-prefixed JSON frames -- the same framing
//! as the raft control plane. Three exchanges:
//!
//! 1. `Prepare { txn_id, coordinator, commit_ts, read_ts, entries }`
//!    -> `Vote { yes, reason }`. The coordinator allocates ONE
//!    cluster-global ts range, pre-encodes every row version (header
//!    0x02, final commit ts inside the key) and every unique-index
//!    reservation, then groups them by slot-owner and asks each
//!    remote owner. A participant validates (write-write conflict:
//!    newest version ts of the row > read_ts, except the txn's own
//!    re-prepared ts; unique value owned by a different pk) and on YES
//!    writes ONE atomic RocksDB batch: prepared rows + unique
//!    reservations + an in-doubt marker `sql2pc/<txn_id>`.
//! 2. `Decide { txn_id, commit, index_ops }` -> `Ack`. The coordinator
//!    durably records the outcome FIRST (`sql2pc_out/<txn_id>`, a
//!    plain key outside every slot prefix), then tells every
//!    participant. Commit flips the prepared header 0x02 -> 0x01/0x00
//!    in place (atomic visibility), applies the secondary-index ops
//!    and drops the marker; abort deletes the prepared keys, unique
//!    reservations and marker. Both are idempotent.
//! 3. `TxnStatus { txn_id, node }` -> `Status { Committed{ops} |
//!    Aborted | Unknown }` -- a participant asking any node what
//!    became of an in-doubt txn (mainly for tests; recovery uses
//!    HTTP below).
//!
//! ## In-doubt txns and the lease
//! A crash between the two phases leaves markers behind. Recovery
//! (see [`recover`]) runs at startup and every ~30s: for each marker
//! it asks the coordinator's HTTP control API
//! `/sql2pc/status?id=<txn_id>&node=<resp_addr>`:
//!
//! - Committed -> finish the commit locally (the response carries
//!   this participant's index ops, so a lost Decide is recoverable);
//! - Aborted -> run the abort batch;
//! - Unknown / unreachable AND the marker is older than the 60s
//!   lease -> abort locally. The outcome record is written durably
//!   BEFORE the first Decide, so "no outcome anywhere" means no
//!   participant was ever told to commit: aborting is always safe
//!   once the coordinator has had a full lease interval to surface an
//!   outcome and failed to.
//!
//! Outcome records older than ~5 minutes are garbage-collected; by
//! then every participant has either applied the decision or will
//! time out its own lease and abort.
//!
//! ## Visibility
//! Prepared versions carry header 0x02 and are skipped by every
//! snapshot reader (`row::is_prepared`), so an in-flight txn never
//! shadows older committed versions; the commit flip makes all its
//! rows visible at once.

pub mod client;
pub mod gather;
pub mod participant;
pub mod plan;
pub mod proto;
pub mod recover;
pub mod server;
pub mod twopc;

use crate::router::{self, RouteDecision};
use crate::state::Shared;

/// Lease a prepared txn holds on its participants: past this age with
/// no reachable outcome, participants abort on their own.
pub const LEASE_SECS: u64 = 60;
/// Age at which settled outcome records are garbage-collected.
pub const OUTCOME_GC_SECS: u64 = 300;

/// Cluster routing snapshot for one write: slot bands plus this node's
/// RESP address (`stable_addrs` entries are RESP addresses).
#[derive(Clone, Debug, PartialEq)]
pub struct Routing {
    pub addrs: Vec<String>,
    pub per_node_slots: usize,
    pub host: String,
}

/// Routing for `shared` when the cluster is ready, else None (the
/// caller then takes the single-node fast path unchanged).
pub fn routing(shared: &Shared) -> Option<Routing> {
    let topo = shared.topology.read().unwrap();
    if !topo.cluster_ready || topo.stable_addrs.is_empty() {
        return None;
    }
    Some(Routing {
        addrs: topo.stable_addrs.clone(),
        per_node_slots: topo.per_node_slots,
        host: shared.conf.bind.clone(),
    })
}

/// Owning node (RESP address) of `slot`; `None` when no routing.
pub fn owner(r: &Routing, slot: u16) -> String {
    match router::route(slot, &r.addrs, r.per_node_slots, &r.host) {
        RouteDecision::Local => r.host.clone(),
        RouteDecision::Moved { addr, .. } => addr,
    }
}

/// One owner's slice of the slot space for a scatter-gather read:
/// slots `[lo, hi]` (inclusive) belong to `owner` -- the SAME cut
/// `router::route` makes (`slot <= (i+1)*per` per node, the last node
/// absorbing the division remainder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Band {
    pub lo: u16,
    pub hi: u16,
    /// Owner's RESP address (one entry of `Routing.addrs`).
    pub owner: String,
}

/// The disjoint per-owner slot bands covering 0..=16383, in
/// `Routing.addrs` order. `route` binds node i (i < last) to
/// `(i-1)*per < slot <= i*per` -- i.e. `[i*per + 1, (i+1)*per]`, with
/// node 0 starting at 0 -- and the last node absorbs the division
/// remainder through 16383. Every pk maps to exactly one band.
pub fn bands(r: &Routing) -> Vec<Band> {
    let last = r.addrs.len().saturating_sub(1);
    r.addrs
        .iter()
        .enumerate()
        .map(|(i, addr)| {
            let lo = i * r.per_node_slots + usize::from(i > 0);
            let hi = if i == last {
                16383
            } else {
                (i + 1) * r.per_node_slots
            };
            Band {
                lo: lo.min(u16::MAX as usize) as u16,
                hi: hi.min(u16::MAX as usize) as u16,
                owner: addr.clone(),
            }
        })
        .collect()
}

/// Owning node of one physical key: the `<slot>/` prefix decides.
pub fn owner_of_key(r: &Routing, key: &[u8]) -> Option<String> {
    slot_of_key(key).map(|s| owner(r, s))
}

/// Slot prefix of a physical key (`"1234/..."` -> 1234), if well formed.
pub fn slot_of_key(key: &[u8]) -> Option<u16> {
    let slash = key.iter().position(|&b| b == b'/')?;
    let slot: u16 = std::str::from_utf8(&key[..slash]).ok()?.parse().ok()?;
    Some(slot)
}

/// Wall-clock seconds since the unix epoch (marker/outcome bookkeeping).
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::SLOT_NUMBER;

    fn routing3(host: &str) -> Routing {
        Routing {
            addrs: ["a:1", "b:2", host].iter().map(|s| s.to_string()).collect(),
            per_node_slots: SLOT_NUMBER / 3,
            host: host.to_string(),
        }
    }

    #[test]
    fn owner_covers_slot_bands_and_remainder() {
        let r = routing3("c:3");
        assert_eq!(owner(&r, 0), "a:1");
        assert_eq!(owner(&r, SLOT_NUMBER as u16 - 1), "c:3");
        // Bands are inclusive: node0 keeps slot 5461, node1 starts at 5462.
        assert_eq!(owner(&r, 5461), "a:1");
        assert_eq!(owner(&r, 5462), "b:2");
        assert_eq!(owner(&r, 10922), "b:2");
        assert_eq!(owner(&r, 10923), "c:3");
    }

    #[test]
    fn slot_of_key_parses_prefix() {
        assert_eq!(slot_of_key(b"12/x"), Some(12));
        assert_eq!(slot_of_key(b"16383/"), Some(16383));
        assert_eq!(slot_of_key(b"x"), None);
        assert_eq!(slot_of_key(b"nonslot/"), None);
    }

    #[test]
    fn owner_of_key_uses_key_slot() {
        let r = routing3("c:3");
        let mut key = b"0/".to_vec();
        key.extend_from_slice(&[0x20]);
        assert_eq!(owner_of_key(&r, &key), Some("a:1".to_string()));
    }

    #[test]
    fn bands_partition_slots_with_remainder_on_last_owner() {
        let r = routing3("b:2");
        let bs = bands(&r);
        assert_eq!(bs.len(), 3);
        assert_eq!(
            bs.iter().map(|b| (b.lo, b.hi)).collect::<Vec<_>>(),
            vec![(0, 5461), (5462, 10922), (10923, 16383)],
            "inclusive bounds, remainder rides the last band"
        );
        // Every band's owner is what router::route says for both ends.
        for b in &bs {
            assert_eq!(owner(&r, b.lo), b.owner);
            assert_eq!(owner(&r, b.hi), b.owner);
        }
        // Disjoint and complete: concatenating covers 0..=16383 once.
        let mut all: Vec<u16> = bs.iter().flat_map(|b| b.lo..=b.hi).collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n);
        assert_eq!(all.first(), Some(&0));
        assert_eq!(all.last(), Some(&16383));
    }

    #[test]
    fn bands_single_owner_covers_everything() {
        let r = Routing {
            addrs: vec!["solo:1".to_string()],
            per_node_slots: SLOT_NUMBER,
            host: "solo:1".to_string(),
        };
        assert_eq!(
            bands(&r),
            vec![Band {
                lo: 0,
                hi: 16383,
                owner: "solo:1".to_string()
            }]
        );
    }
}
