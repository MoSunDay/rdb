//! Participant logic of the SQL 2PC: pure functions over the store,
//! no in-memory txn state. All 2PC state is RocksDB:
//!
//! - prepared row versions (header 0x02 values under their final
//!   version keys) and unique reservations, written by one atomic
//!   Prepare batch together with an in-doubt marker;
//! - a local outcome record per decided txn (`sql2pc_out/<id>`),
//!   written atomically WITH the decision application so a restart
//!   never re-asks about a txn this node already finished.
//!
//! Every batch here is idempotent: a retried Prepare re-stages the
//! same bytes, a retried Decide finds the marker gone (already
//! applied) and just re-records the outcome.

use std::collections::BTreeMap;

use rocksdb::WriteBatch;
use serde::{Deserialize, Serialize};

use super::proto::{Entry, EntryKind, Outcome, WireOp};
use super::{now_secs, LEASE_SECS};
use crate::sql::storage::row;
use crate::store::{ops, Store};

/// In-doubt marker prefix (plain key, outside every slot prefix).
pub const MARKER_PREFIX: &str = "sql2pc/";
/// Local outcome record prefix (coordinator AND participants).
pub const OUTCOME_PREFIX: &str = "sql2pc_out/";

/// Marker payload: everything recovery needs to finish or abort.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Marker {
    /// Coordinator HTTP address (`/sql2pc/status`).
    pub coordinator: String,
    pub read_ts: u64,
    pub commit_ts: u64,
    /// Wall-clock seconds at prepare time (lease base).
    pub started_at: u64,
    /// Every key the prepare batch staged (rows + unique entries).
    pub keys: Vec<Vec<u8>>,
}

/// Outcome payload. The coordinator's copy carries the per-participant
/// index ops (so recovery answers can finish a lost Decide); a
/// participant's copy only records the decision it applied and its
/// own ops (to answer TxnStatus).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OutcomeRecord {
    pub commit: bool,
    pub commit_ts: u64,
    pub written_at: u64,
    /// RESP address -> that node's index ops (coordinator copy).
    #[serde(default)]
    pub index_ops: BTreeMap<String, Vec<WireOp>>,
    /// This node's own index ops (participant copy).
    #[serde(default)]
    pub own_ops: Vec<WireOp>,
}

pub fn marker_key(txn_id: &str) -> Vec<u8> {
    format!("{MARKER_PREFIX}{txn_id}").into_bytes()
}

pub fn outcome_key(txn_id: &str) -> Vec<u8> {
    format!("{OUTCOME_PREFIX}{txn_id}").into_bytes()
}

pub fn read_marker(store: &Store, txn_id: &str) -> Option<Marker> {
    ops::get_physical(store, &marker_key(txn_id))
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

pub fn read_outcome(store: &Store, txn_id: &str) -> Option<OutcomeRecord> {
    ops::get_physical(store, &outcome_key(txn_id))
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
}

/// Whether `marker` may be aborted without asking anyone: its lease
/// has fully expired.
pub fn lease_expired(m: &Marker) -> bool {
    now_secs().saturating_sub(m.started_at) > LEASE_SECS
}

/// PREPARE verdict: `No` carries the veto reason (a `conflict:` /
/// `dup:` prefixed string the coordinator maps to a client error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Vote {
    Yes,
    No(String),
}

/// PREPARE: validate every entry, then stage one atomic batch.
///
/// Veto reasons are prefixed strings the coordinator maps to client
/// errors: `conflict:` -> 1213, `dup:` -> 1062.
pub fn vote(
    store: &Store,
    txn_id: &str,
    coordinator: &str,
    commit_ts: u64,
    read_ts: u64,
    entries: &[Entry],
) -> Result<Vote, String> {
    for e in entries {
        match e.kind {
            EntryKind::RowPrepared => {
                let Some((_slot, table_id, pk, ts)) = row::parse_version_key(&e.key) else {
                    return Err("conflict: unparsable row key".into());
                };
                if let Some(n) = crate::sql::tx::session::newest_version_ts(store, table_id, &pk)? {
                    // `n == commit_ts` with a prepared value = our own
                    // retried prepare (the ts range is globally unique,
                    // nobody else can own that ts).
                    let own_retry = n == commit_ts || n == ts;
                    if n > read_ts && !own_retry {
                        return Ok(Vote::No(format!(
                            "conflict: write-write conflict on row committed at ts {n} \
                             after read ts {read_ts}"
                        )));
                    }
                }
            }
            EntryKind::UniquePut => {
                if let Some(owner) = ops::get_physical(store, &e.key)?.filter(|v| !v.is_empty()) {
                    if owner != e.value {
                        return Ok(Vote::No(
                            "dup: unique value already owned by another row".to_string(),
                        ));
                    }
                }
            }
            EntryKind::UniqueDel => {}
        }
    }
    let mut batch = WriteBatch::default();
    let mut keys = Vec::with_capacity(entries.len());
    for e in entries {
        match e.kind {
            EntryKind::UniqueDel => batch.delete(&e.key),
            _ => batch.put(&e.key, &e.value),
        }
        keys.push(e.key.clone());
    }
    let marker = serde_json::to_vec(&Marker {
        coordinator: coordinator.to_string(),
        read_ts,
        commit_ts,
        started_at: now_secs(),
        keys,
    })
    .map_err(|e| e.to_string())?;
    batch.put(marker_key(txn_id), marker);
    ops::batch_write(store, batch).map(|_| Vote::Yes)
}

/// DECIDE (and recovery's replay of one): apply the decision
/// atomically -- flips, index ops, marker removal and the local
/// outcome record share one batch. Idempotent. Returns the highest
/// version ts this decision made visible (0 when this node had no
/// marker or staged no rows): a participant raises its read point to
/// it, or snapshots taken below it would never see the flipped rows.
pub fn decide(
    store: &Store,
    txn_id: &str,
    commit: bool,
    index_ops: &[WireOp],
) -> Result<u64, String> {
    let marker = read_marker(store, txn_id);
    let mut batch = WriteBatch::default();
    // Highest version ts this decision makes visible (0 when this node
    // staged no rows): the caller raises its read point to it.
    let mut hi = marker.as_ref().map(|m| m.commit_ts).unwrap_or(0);
    if commit {
        if let Some(m) = &marker {
            for key in &m.keys {
                if let Some(v) = ops::get_physical(store, key)? {
                    if row::is_prepared(&v) {
                        let mut final_v = v;
                        final_v[0] = row::final_header(&final_v);
                        batch.put(key, final_v);
                        if let Some((.., ts)) = row::parse_version_key(key) {
                            hi = hi.max(ts);
                        }
                    }
                }
            }
        }
        for (key, val) in index_ops {
            match val {
                Some(v) => batch.put(key, v),
                None => batch.delete(key),
            }
        }
    } else if let Some(m) = &marker {
        for key in &m.keys {
            batch.delete(key);
        }
    }
    batch.delete(marker_key(txn_id));
    let rec = OutcomeRecord {
        commit,
        commit_ts: marker.as_ref().map(|m| m.commit_ts).unwrap_or(0),
        written_at: now_secs(),
        index_ops: BTreeMap::new(),
        own_ops: index_ops.to_vec(),
    };
    batch.put(
        outcome_key(txn_id),
        serde_json::to_vec(&rec).map_err(|e| e.to_string())?,
    );
    ops::batch_write(store, batch).map(|_| hi)
}

/// TxnStatus answer from THIS node's records: a local outcome wins,
/// an in-doubt marker means Unknown, neither means the node never
/// heard of the txn (Unknown -- safe, the asker's lease timer runs).
pub fn status(store: &Store, txn_id: &str, _node: &str) -> Outcome {
    match read_outcome(store, txn_id) {
        Some(rec) if rec.commit => Outcome::Committed {
            index_ops: rec.own_ops,
        },
        Some(_) => Outcome::Aborted,
        None => Outcome::Unknown,
    }
}

/// Outcome record a coordinator writes before disseminating its
/// decision (index ops keyed by participant RESP address).
pub fn coordinator_outcome(
    commit: bool,
    commit_ts: u64,
    index_ops: BTreeMap<String, Vec<WireOp>>,
) -> OutcomeRecord {
    OutcomeRecord {
        commit,
        commit_ts,
        written_at: now_secs(),
        index_ops,
        own_ops: Vec::new(),
    }
}

/// Every in-doubt marker on this store (recovery sweep input).
pub fn markers(store: &Store) -> Vec<(String, Marker)> {
    ops::prefix_iter_collect(store, MARKER_PREFIX.as_bytes(), 10_000)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| {
            let id = String::from_utf8_lossy(&k[MARKER_PREFIX.len()..]).into_owned();
            serde_json::from_slice(&v).ok().map(|m| (id, m))
        })
        .collect()
}

/// Every outcome record on this store (GC sweep input).
pub fn outcomes(store: &Store) -> Vec<(String, OutcomeRecord)> {
    ops::prefix_iter_collect(store, OUTCOME_PREFIX.as_bytes(), 100_000)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| {
            let id = String::from_utf8_lossy(&k[OUTCOME_PREFIX.len()..]).into_owned();
            serde_json::from_slice(&v).ok().map(|m| (id, m))
        })
        .collect()
}
