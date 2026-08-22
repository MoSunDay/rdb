//! Building a 2PC commit plan: the coordinator's write set pre-encoded
//! into protocol entries, grouped by slot-owner.
//!
//! Both commit paths feed the SAME builder: explicit BEGIN..COMMIT
//! txns (`try_plan_txn`) and autocommit statements
//! (`try_plan_simple`). Both return `None` unless the cluster is
//! ready AND some participant is another node -- otherwise the caller
//! keeps the exact single-node M1/M2 batch path.
//!
//! The timestamp range is allocated BEFORE encoding (cluster-global,
//! so `ts.start` is unique across nodes and names the txn), and row
//! versions are keyed with their final commit ts right away: a
//! participant only swaps the value header, never re-stamps.

use std::collections::BTreeMap;

use super::participant;
use crate::sql::dist::proto::{Entry, EntryKind, WireOp};
use crate::sql::dist::{owner_of_key, routing, Routing};
use crate::sql::index::IndexOps;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::row;
use crate::sql::storage::schema::{TableSchema, Value};
use crate::sql::tx::{Txn, TxnWrite};
use crate::state::Shared;

/// One write as the planner sees it: `(table, pk, new row values)`;
/// `None` values mean tombstone.
type PlanWrite<'a> = (u32, Vec<u8>, Option<&'a Vec<Value>>);

/// Writes of one statement in timestamp-consumption order:
/// `(pk, live row or tombstone)`. One version each; the caller has
/// already collapsed intra-txn overwrites (txn buffer) or widened the
/// pk-moving UPDATE case into two entries.
pub type SimpleWrites<'a> = Vec<(Vec<u8>, Option<&'a Vec<Value>>)>;

/// One participant's slice of a commit plan.
#[derive(Clone, Debug, Default)]
pub struct ParticipantPlan {
    /// Prepared row versions + unique reservations (the Prepare batch).
    pub entries: Vec<Entry>,
    /// Secondary-index ops applied only at Decide{commit}.
    pub index_ops: Vec<WireOp>,
}

/// A fully-built 2PC commit: entries + per-participant grouping.
#[derive(Clone, Debug)]
pub struct CommitPlan {
    /// Globally unique txn name (= the allocated ts range start).
    pub txn_id: String,
    pub read_ts: u64,
    pub commit_ts: u64,
    /// Highest ts granted to this txn (`ts.end - 1`): rows spread
    /// across participants carry consecutive ts values up to here, and
    /// every participant must advance its read point past the WHOLE
    /// txn (scatter-gather reads on any node see all its rows).
    pub watermark: u64,
    /// Coordinator HTTP address participants recover through.
    pub coordinator_http: String,
    /// RESP address -> that node's slice (the coordinator itself
    /// included when it owns slots of the write).
    pub participants: BTreeMap<String, ParticipantPlan>,
}

impl CommitPlan {
    pub fn has_remote(&self, host: &str) -> bool {
        self.participants.keys().any(|a| a != host)
    }
}

/// Plan an explicit txn's commit; `None` keeps the local path. The
/// ts range is allocated here (cluster mode only).
pub fn try_plan_txn(shared: &Shared, txn: &Txn) -> SqlResult<Option<CommitPlan>> {
    let Some(r) = routing(shared) else {
        return Ok(None);
    };
    let schemas = crate::sql::tx::session::written_schemas(shared, txn);
    let idx = crate::sql::tx::session::commit_index_ops(shared, txn, &schemas)?;
    let mut writes: Vec<PlanWrite> = Vec::with_capacity(txn.writes.len());
    for ((table_id, pk), w) in &txn.writes {
        writes.push((
            *table_id,
            pk.clone(),
            match w {
                TxnWrite::Row(values) => Some(values),
                TxnWrite::Tombstone => None,
            },
        ));
    }
    build(shared, &r, txn.read_ts, &schemas, &writes, &idx)
}

/// Plan one autocommit statement's decided writes; `None` keeps the
/// local path. `read_ts` is the statement's snapshot (`now()`).
pub fn try_plan_simple(
    shared: &Shared,
    read_ts: u64,
    schema: &TableSchema,
    writes: &SimpleWrites<'_>,
    idx: &IndexOps,
) -> SqlResult<Option<CommitPlan>> {
    let Some(r) = routing(shared) else {
        return Ok(None);
    };
    let schemas = BTreeMap::from([(schema.id, schema.clone())]);
    let full: Vec<PlanWrite> = writes
        .iter()
        .map(|(pk, values)| (schema.id, pk.clone(), *values))
        .collect();
    build(shared, &r, read_ts, &schemas, &full, idx)
}

/// Core builder: one version per write (ts assigned in write order),
/// unique reservations split out of the index ops, everything grouped
/// by the slot-owner of its key. `None` when no participant is
/// another node.
fn build(
    shared: &Shared,
    r: &Routing,
    read_ts: u64,
    schemas: &BTreeMap<u32, TableSchema>,
    writes: &[PlanWrite],
    idx: &IndexOps,
) -> SqlResult<Option<CommitPlan>> {
    let ts = shared.sql_ts.alloc_n(writes.len() as u64);
    let mut participants: BTreeMap<String, ParticipantPlan> = BTreeMap::new();
    for (i, (table_id, pk, values)) in writes.iter().enumerate() {
        let schema = schemas.get(table_id).ok_or_else(|| {
            SqlError::new(
                ErrorCode::NoSuchTable,
                format!("table {table_id} left the catalog mid-txn"),
            )
        })?;
        let slot = row::row_slot(schema, pk);
        let key = row::version_key(schema, slot, pk, ts.start + i as u64);
        let final_val = match values {
            Some(vs) => row::encode_row(schema, vs).map_err(SqlError::from)?,
            None => row::encode_tombstone(),
        };
        add_entry(
            &mut participants,
            r,
            key.clone(),
            Entry {
                key,
                value: row::prepared_value(&final_val),
                kind: EntryKind::RowPrepared,
            },
        );
    }
    let mut secondary: Vec<WireOp> = Vec::new();
    for (key, val) in idx.iter().cloned() {
        let unique = crate::sql::index::keys::parse_index_key(&key).map(|(kind, ..)| kind)
            == Some(crate::sql::storage::codec::KIND_SQL_UNIQUE_INDEX);
        if unique {
            let kind = if val.is_some() {
                EntryKind::UniquePut
            } else {
                EntryKind::UniqueDel
            };
            add_entry(
                &mut participants,
                r,
                key.clone(),
                Entry {
                    key,
                    value: val.clone().unwrap_or_default(),
                    kind,
                },
            );
        } else {
            // Secondary (0x21) ops -- and anything unparseable, kept
            // verbatim -- ride the commit decision.
            secondary.push((key, val));
        }
    }
    for (key, val) in secondary {
        if let Some(addr) = owner_of_key(r, &key) {
            participants
                .entry(addr)
                .or_default()
                .index_ops
                .push((key, val));
        }
    }
    if !participants.keys().any(|a| a != &r.host) {
        return Ok(None);
    }
    Ok(Some(CommitPlan {
        txn_id: format!("ts{}", ts.start),
        read_ts,
        commit_ts: ts.start,
        watermark: ts.end.saturating_sub(1),
        coordinator_http: shared.conf.http_address.clone(),
        participants,
    }))
}

/// Route one entry to its slot-owner's bucket.
fn add_entry(
    participants: &mut BTreeMap<String, ParticipantPlan>,
    r: &Routing,
    key: Vec<u8>,
    entry: Entry,
) {
    if let Some(addr) = owner_of_key(r, &key) {
        participants.entry(addr).or_default().entries.push(entry);
    }
}

/// Marker key of one txn (in-doubt bookkeeping on participants).
pub fn marker_key(txn_id: &str) -> Vec<u8> {
    format!("{}{}", participant::MARKER_PREFIX, txn_id).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_key_shape() {
        assert_eq!(marker_key("ts9"), b"sql2pc/ts9".to_vec());
    }
}
