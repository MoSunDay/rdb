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

use std::collections::{BTreeMap, BTreeSet};

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

/// Outcome payload. Both roles map index ops per node: the
/// coordinator's copy carries every participant's slice (so recovery
/// answers can finish a lost Decide); a participant's copy carries its
/// own slice under its bind (so status answers serve per-node ops
/// without ever leaking another node's slice). `own_ops` is
/// local-replay-only state: this node's slice on a participant.
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

/// Extract the conflicting commit's ts from a `conflict:` veto reason
/// (the format written by `vote`: "... committed at ts N after read ts
/// R"). A veto proves the coordinator's read point lags a decided
/// commit; the coordinator folds N into its timestamp oracle so the
/// client's retry pins a snapshot above the row that vetoed it instead
/// of re-pinning the same lagging view and vetoing forever.
pub fn conflict_ts(reason: &str) -> Option<u64> {
    const TAG: &str = "committed at ts ";
    let rest = reason.split_once(TAG)?.1;
    let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// PREPARE: validate every entry, then stage one atomic batch.
///
/// Veto reasons are prefixed strings the coordinator maps to client
/// errors: `conflict:` -> 1213, `dup:` -> 1062. A columnar segment is
/// vetoed (`schema mismatch ...`) when the local catalog disagrees.
#[allow(clippy::too_many_arguments)] // explicit handles beat a param struct here
pub fn vote(
    store: &Store,
    raft: &std::sync::RwLock<crate::state::RaftState>,
    dir: &std::path::Path,
    txn_id: &str,
    coordinator: &str,
    commit_ts: u64,
    read_ts: u64,
    entries: &[Entry],
    segments: &[crate::sql::columnar::commit::PendingSegment],
) -> Result<Vote, String> {
    // Unique-entry validation mirrors the local `check_unique`: keys
    // this batch VACATES (UniqueDel) are exempt from the store check
    // (their on-disk owner is leaving in the same batch, e.g. a value
    // swap between two rows of one UPDATE), and two different pks
    // claiming the SAME key inside this batch are a duplicate.
    let vacated: BTreeSet<&[u8]> = entries
        .iter()
        .filter(|e| e.kind == EntryKind::UniqueDel)
        .map(|e| e.key.as_slice())
        .collect();
    let mut claimed: BTreeMap<&[u8], &[u8]> = BTreeMap::new();
    for e in entries {
        match e.kind {
            EntryKind::RowPrepared => {
                let Some((_slot, table_id, pk, ts)) = row::parse_version_key(&e.key) else {
                    return Err("conflict: unparsable row key".into());
                };
                let pn = crate::sql::tx::session::newest_version_ts(store, table_id, &pk)?;
                if let Some(n) = pn {
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
                if let Some(first) = claimed.get(e.key.as_slice()) {
                    if *first != e.value.as_slice() {
                        return Ok(Vote::No(
                            "dup: unique value already owned by another row".to_string(),
                        ));
                    }
                } else {
                    claimed.insert(e.key.as_slice(), e.value.as_slice());
                }
                if vacated.contains(e.key.as_slice()) {
                    continue; // the batch itself removes the old owner
                }
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
    // Columnar segments: the local catalog must agree (same table id
    // and columns, columnar engine) before any file is written.
    for seg in segments {
        let local = crate::sql::storage::catalog::lookup_raft(raft, &seg.schema.name)
            .map_err(|e| format!("error: catalog lookup failed: {e}"))?;
        let matches = local
            .map(|s| {
                s.engine.is_columnar() && s.id == seg.schema.id && s.columns == seg.schema.columns
            })
            .unwrap_or(false);
        if !matches {
            return Ok(Vote::No(format!(
                "schema mismatch for columnar table '{}'",
                seg.schema.name
            )));
        }
    }
    let mut batch = WriteBatch::default();
    let mut keys = Vec::with_capacity(entries.len() + segments.len());
    for e in entries {
        match e.kind {
            EntryKind::UniqueDel => batch.delete(&e.key),
            _ => batch.put(&e.key, &e.value),
        }
        keys.push(e.key.clone());
    }
    // Segment files are created BEFORE the marker write: a crash in
    // between leaves an orphan file (swept in M5), never a dangling
    // meta. Metas stage as Prepared; decide(commit) flips them Live.
    for seg in segments {
        let (file, columns, num_rows) = crate::sql::columnar::writer::write_segment_file(
            dir,
            &seg.schema,
            seg.segment_id,
            &seg.rows,
        )
        .map_err(|e| format!("error: columnar flush failed: {e}"))?;
        let meta = crate::sql::columnar::writer::build_meta(
            &seg.schema,
            seg.segment_id,
            seg.commit_ts,
            crate::sql::columnar::meta::SegmentState::Prepared,
            file,
            columns,
            num_rows,
        );
        let key = crate::sql::columnar::meta::meta_key(meta.table_id, meta.segment_id);
        let encoded = crate::sql::columnar::meta::encode_meta(&meta)
            .map_err(|e| format!("error: columnar flush failed: {e}"))?;
        batch.put(&key, encoded);
        keys.push(key);
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
    // Same-batch ts floor (restart clock fencing, see tx::floor): the
    // staged versions carry ts values up to the coordinator's
    // watermark; a crash between Prepare and Decide still leaves the
    // floor above every staged ts, so the recovery replay (or the
    // lease-expiry abort) can never rewind the clock past them. Only
    // stamped when something ts-bearing is staged: an index-ops-only
    // slice stamps nothing (0 would REGRESS the persisted floor).
    let hi = staged_max_ts(entries, segments);
    if hi > 0 {
        crate::sql::tx::floor::stamp(&mut batch, hi);
    }
    ops::batch_write(store, batch).map(|_| Vote::Yes)
}

/// Highest ts any staged record in a Prepare batch carries: the max of
/// the row version keys' ts suffixes and the columnar segments' commit
/// ts (the coordinator's `commit_ts` argument is only the range START).
fn staged_max_ts(
    entries: &[Entry],
    segments: &[crate::sql::columnar::commit::PendingSegment],
) -> u64 {
    let mut hi = segments.iter().map(|s| s.commit_ts).max().unwrap_or(0);
    for e in entries {
        if e.kind == EntryKind::RowPrepared {
            if let Some((.., ts)) = row::parse_version_key(&e.key) {
                hi = hi.max(ts);
            }
        }
    }
    hi
}

/// DECIDE (and recovery's replay of one): apply the decision
/// atomically -- flips, segment-meta flips, index ops, marker removal
/// and the local outcome record share one batch. Idempotent. Returns
/// the highest version ts this decision made visible (0 when this node
/// had no marker or staged no rows): a participant raises its read
/// point to it, or snapshots taken below it would never see the
/// flipped rows.
pub fn decide(
    store: &Store,
    dir: &std::path::Path,
    registry: &crate::sql::columnar::Registry,
    txn_id: &str,
    bind: &str,
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
                if crate::sql::columnar::meta::parse_meta_key(key).is_some() {
                    // Segment meta: flip Prepared -> Live in the same
                    // batch. `hi` must cover EVERY ts this decision
                    // makes visible -- row versions AND columnar
                    // segment tails: a mixed slice (remote rows plus a
                    // locally staged segment) can stage a segment whose
                    // commit_ts sits ABOVE the marker's commit_ts, so
                    // the marker alone is no upper bound for `hi`.
                    if let Some(v) = ops::get_physical(store, key)? {
                        if let Ok(mut meta) = crate::sql::columnar::meta::decode_meta(&v) {
                            hi = hi.max(meta.commit_ts);
                            if meta.state == crate::sql::columnar::meta::SegmentState::Prepared {
                                meta.state = crate::sql::columnar::meta::SegmentState::Live;
                                if let Ok(enc) = crate::sql::columnar::meta::encode_meta(&meta) {
                                    batch.put(key, enc);
                                }
                            }
                            registry.insert(&meta);
                        }
                    }
                    continue;
                }
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
            // Segment metas also vanish from the registry and disk:
            // best-effort (the batch delete below is the source of
            // truth; file cleanup only avoids orphans eagerly).
            if let Some((table_id, segment_id)) = crate::sql::columnar::meta::parse_meta_key(key) {
                if let Ok(Some(v)) = ops::get_physical(store, key) {
                    if let Ok(meta) = crate::sql::columnar::meta::decode_meta(&v) {
                        registry.remove(table_id, &[segment_id]);
                        let _ = std::fs::remove_file(dir.join(meta.file));
                    }
                }
            }
            batch.delete(key);
        }
    }
    batch.delete(marker_key(txn_id));
    let rec = OutcomeRecord {
        commit,
        commit_ts: marker.as_ref().map(|m| m.commit_ts).unwrap_or(0),
        written_at: now_secs(),
        // The slice is mapped under THIS node's bind so every status
        // answer serves ops per requesting node; `own_ops` stays
        // local-replay-only and never crosses the wire. (Binaries
        // before this format wrote an empty map here; those records
        // answer "no mapped ops" to everyone -- degraded replay, but
        // no foreign slice can ever be applied.)
        index_ops: if commit {
            BTreeMap::from([(bind.to_string(), index_ops.to_vec())])
        } else {
            BTreeMap::new()
        },
        own_ops: index_ops.to_vec(),
    };
    batch.put(
        outcome_key(txn_id),
        serde_json::to_vec(&rec).map_err(|e| e.to_string())?,
    );
    // Same-batch ts floor, only when this decision made something
    // visible: hi == 0 means the node had no marker (idempotent replay
    // or a foreign txn) and the batch is bookkeeping only -- stamping 0
    // would regress the persisted floor below earlier commits.
    if commit && hi > 0 {
        crate::sql::tx::floor::stamp(&mut batch, hi);
    }
    ops::batch_write(store, batch).map(|_| hi)
}

/// TxnStatus answer from THIS node's records: a local outcome wins,
/// an in-doubt marker means Unknown, neither means the node never
/// heard of the txn (Unknown -- safe, the asker's lease timer runs).
/// A committed answer carries only the slice mapped for `node`.
pub fn status(store: &Store, txn_id: &str, node: &str) -> Outcome {
    match read_outcome(store, txn_id) {
        Some(rec) if rec.commit => Outcome::Committed {
            // Coordinator records map every participant, participant
            // records map their own bind; `own_ops` never crosses the
            // wire (this surface cannot verify the requester owns it).
            index_ops: rec.index_ops.get(node).cloned().unwrap_or_default(),
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

/// Every key referenced by any in-doubt marker (columnar GC sweep
/// input): these belong to a 2PC txn that has not decided yet and
/// must never be swept. Undecodable markers contribute nothing.
pub fn in_doubt_keys(store: &Store) -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = markers(store)
        .into_iter()
        .flat_map(|(_, marker)| marker.keys)
        .collect();
    keys.sort();
    keys.dedup();
    keys
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

#[cfg(test)]
mod tests {
    use super::{conflict_ts, decide, marker_key, Marker};
    use crate::sql::columnar::meta::{self, SegmentMeta, SegmentState};
    use crate::sql::columnar::Registry;
    use crate::sql::tx::floor;
    use crate::store::ops;

    use rocksdb::WriteBatch;

    /// Lightest faithful scaffolding (same shape as the `floor` tests):
    /// a real RocksDB `Store` on a tempdir, no raft/catalog around it.
    fn open_store() -> (tempfile::TempDir, crate::store::Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::store::open(
            crate::store::data_path(dir.path().to_str().unwrap(), "t")
                .to_str()
                .expect("utf8 path"),
        )
        .expect("open store");
        (dir, store)
    }

    fn staged_segment(table_id: u32, segment_id: u64, commit_ts: u64) -> SegmentMeta {
        SegmentMeta {
            table_id,
            table_name: format!("t{table_id}"),
            segment_id,
            commit_ts,
            state: SegmentState::Prepared,
            num_rows: 1,
            file: format!("{table_id}_{segment_id}.bin"),
            columns: vec![],
        }
    }

    /// Stage one segment meta (Prepared) plus its in-doubt marker in a
    /// single atomic batch -- the same `(meta_key, encode_meta)` /
    /// `marker_key` + serde_json write paths `vote` uses.
    fn stage_marker_with_segment(
        store: &crate::store::Store,
        txn_id: &str,
        seg: &SegmentMeta,
        commit_ts: u64,
    ) -> Vec<u8> {
        let key = meta::meta_key(seg.table_id, seg.segment_id);
        let mut batch = WriteBatch::default();
        batch.put(&key, meta::encode_meta(seg).expect("encode meta"));
        batch.put(
            marker_key(txn_id),
            serde_json::to_vec(&Marker {
                coordinator: "http://127.0.0.1:9".into(),
                read_ts: commit_ts.saturating_sub(1),
                commit_ts,
                started_at: 0,
                keys: vec![key.clone()],
            })
            .expect("marker json"),
        );
        ops::batch_write(store, batch).expect("stage prepare batch");
        key
    }

    #[test]
    fn conflict_ts_extracts_committed_ts_from_veto_reason() {
        let reason = "conflict: write-write conflict on row committed at ts 4202538 \
                      after read ts 4202537";
        assert_eq!(conflict_ts(reason), Some(4202538));
        // Trailing digits (no "after read ts" tail) still parse.
        assert_eq!(
            conflict_ts("conflict: write-write conflict on row committed at ts 7"),
            Some(7)
        );
    }

    #[test]
    fn conflict_ts_rejects_non_conflict_reasons() {
        assert_eq!(conflict_ts("dup: duplicate entry for key"), None);
        assert_eq!(conflict_ts("conflict: unparsable row key"), None);
        assert_eq!(
            conflict_ts("conflict: write-write conflict on row committed at ts "),
            None
        );
        assert_eq!(conflict_ts(""), None);
    }

    /// Mixed-slice regression: a locally staged columnar segment can
    /// carry commit_ts ABOVE the coordinator's commit point (90 here,
    /// segment 100). decide(commit) makes that segment Live, so both
    /// the returned `hi` and the persisted floor must cover 100 --
    /// otherwise a kill -9 reboot starts the oracle under the segment
    /// and it is briefly invisible (floor must be >= any Live ts).
    #[test]
    fn decide_commit_hi_covers_segment_ts_above_marker_commit_ts() {
        let (dir, store) = open_store();
        let seg = staged_segment(7, 11, 100);
        let key = stage_marker_with_segment(&store, "t-mixed", &seg, 90);

        let registry = Registry::default();
        let hi = decide(
            &store,
            dir.path(),
            &registry,
            "t-mixed",
            "127.0.0.1:1",
            /* commit */ true,
            &[],
        )
        .expect("decide");

        assert_eq!(
            hi, 100,
            "hi must reach the segment's commit_ts, not stop at the marker's 90"
        );
        // Persisted floor: decoded from FLOOR_KEY via the boot path.
        assert!(
            floor::recover(&store) >= 100,
            "floor {} must not sit below a segment this decision made Live",
            floor::recover(&store)
        );
        // The flip itself is unchanged: Prepared -> Live on disk and in
        // the registry.
        let raw = ops::get_physical(&store, &key)
            .expect("get meta")
            .expect("meta present");
        assert_eq!(
            meta::decode_meta(&raw).expect("decode meta").state,
            SegmentState::Live
        );
        assert_eq!(
            registry.segments(7).as_slice(),
            &[{
                let mut m = seg.clone();
                m.state = SegmentState::Live;
                m
            }]
        );
        drop(store);
        drop(dir);
    }

    /// The raise is a max, never a regression: a segment staged BELOW
    /// the marker's commit_ts leaves `hi` at the marker's ts.
    #[test]
    fn decide_commit_hi_stays_at_marker_ts_when_segment_is_lower() {
        let (dir, store) = open_store();
        let seg = staged_segment(8, 21, 50);
        let key = stage_marker_with_segment(&store, "t-low", &seg, 90);

        let registry = Registry::default();
        let hi = decide(
            &store,
            dir.path(),
            &registry,
            "t-low",
            "127.0.0.1:1",
            /* commit */ true,
            &[],
        )
        .expect("decide");

        assert_eq!(hi, 90);
        assert_eq!(floor::recover(&store), 90);
        assert_eq!(
            meta::decode_meta(
                &ops::get_physical(&store, &key)
                    .expect("get meta")
                    .expect("meta present")
            )
            .expect("decode meta")
            .state,
            SegmentState::Live
        );
        drop(store);
        drop(dir);
    }
}
