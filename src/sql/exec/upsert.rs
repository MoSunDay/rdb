//! Explicit INSERT conflict resolution (M2): `ON DUPLICATE KEY
//! UPDATE` and `REPLACE INTO` (`exec::write::insert` dispatches here).
//!
//! Both paths decide their writes PURELY over a conflict snapshot
//! taken before anything lands: the table's live rows (autocommit: a
//! fresh frontier-synced `now()`; txn: the pinned `read_ts` MERGED
//! with the txn's staged writes), indexed pk -> row plus one
//! value-key -> owner map per UNIQUE index. Decisions then flow into
//! the shared row sink `write::apply_writes`, so index migration
//! (including an ODKU update that moves the pk itself), the write
//! frontier probe, 2PC and the local batch are exactly the UPDATE
//! path's machinery.
//!
//! MySQL semantics kept:
//! - conflict probe order is deterministic: the incoming pk first,
//!   then unique indexes in schema (column) order; the first hit wins;
//! - affected rows: insert = 1, ODKU update with change = 2, ODKU
//!   update to identical values = 0 (the row compare happens before
//!   any write), REPLACE = deleted rows + 1;
//! - `VALUES(col)` in an ODKU assignment reads the INCOMING row (the
//!   parser only allows the marker there); other columns read the
//!   EXISTING row;
//! - later rows of one statement see earlier rows' effects: the
//!   overlay AND the unique maps are maintained per decided write
//!   (`overlay_write`), so a value freed earlier in the statement is
//!   simply absent from its map (no stale-snapshot conflicts) and
//!   ownership stays one map lookup (no O(rows^2) overlay scan);
//!
//! AUTO_INCREMENT: ids are allocated for every incoming row BEFORE
//! conflict decisions (same as the plain path), so a row that takes
//! the ODKU update branch burns its reserved id -- the gap semantics
//! MySQL's ODKU has too.
//!
//! Cluster mode: `write::insert` rejects ODKU/REPLACE with ER 1235
//! when the cluster spans remote slot owners (plan decision 1b); the
//! snapshot read here is local-band only by design.

use std::collections::BTreeMap;

use crate::sql::exec::insert_common::{bad_field, subst_values};
use crate::sql::exec::write::{
    apply_assignments, apply_writes, pk_dup_entry, pk_key_of, single_table_scope, RowWrite,
};
use crate::sql::exec::{ExecOutcome, SqlSession};
use crate::sql::index::{dup_entry, keys};
use crate::sql::parse::ast::Expr;
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::schema::{TableSchema, Value};
use crate::sql::tx::Txn;
use crate::state::Shared;

/// Conflict snapshot of one table: live rows by pk plus, per unique
/// index (schema order), encoded value -> owning pk. `overlay` holds
/// THIS statement's decided writes so later incoming rows see earlier
/// ones (pk -> live row, None = deleted).
pub(crate) struct ConflictView {
    read_ts: u64,
    rows: BTreeMap<Vec<u8>, Vec<Value>>,
    uniques: Vec<BTreeMap<Vec<u8>, Vec<u8>>>,
    overlay: BTreeMap<Vec<u8>, Option<Vec<Value>>>,
}

/// (column position, index def) of every UNIQUE index in schema
/// order -- the one ordering shared by `view.uniques`, conflict
/// probes and the preemption checks.
fn unique_indexes(schema: &TableSchema) -> Vec<(usize, &crate::sql::storage::schema::IndexDef)> {
    schema
        .indexes
        .iter()
        .filter(|i| i.unique)
        .filter_map(|i| schema.column_index(&i.column).map(|pos| (pos, i)))
        .collect()
}

/// Build the conflict snapshot (see the module doc for the read point).
fn conflict_view(
    shared: &Shared,
    schema: &TableSchema,
    txn: Option<&Txn>,
) -> SqlResult<ConflictView> {
    let read_ts = match txn {
        Some(t) => t.read_ts,
        None => {
            shared.sql_ts.sync_cursor_frontier();
            shared.sql_ts.now()
        }
    };
    let mut live = crate::sql::exec::scan::visible_rows(&shared.store, schema, read_ts)?;
    if let Some(t) = txn {
        live = crate::sql::tx::merge_rows(schema, live, t)?;
    }
    let mut rows = BTreeMap::new();
    for r in live {
        rows.insert(pk_key_of(schema, &r)?, r);
    }
    let mut uniques = Vec::with_capacity(schema.indexes.len());
    for (pos, _) in unique_indexes(schema) {
        let mut m = BTreeMap::new();
        for (pk, r) in &rows {
            let v = &r[pos];
            if !matches!(v, Value::Null) {
                m.insert(keys::col_key_of(v).map_err(SqlError::from)?, pk.clone());
            }
        }
        uniques.push(m);
    }
    Ok(ConflictView {
        read_ts,
        rows,
        uniques,
        overlay: BTreeMap::new(),
    })
}

/// Live row at `pk`: this statement's overlay first, then the snapshot.
fn row_at<'a>(view: &'a ConflictView, pk: &[u8]) -> Option<&'a Vec<Value>> {
    match view.overlay.get(pk) {
        Some(Some(r)) => Some(r),
        Some(None) => None,
        None => view.rows.get(pk),
    }
}

/// Owning live pk of one unique value: the map IS the live truth
/// (`overlay_write` keeps it in step with every decided write), so
/// this is one lookup -- no overlay scan, no stale snapshot owners.
fn unique_owner(view: &ConflictView, uidx: usize, value: &Value) -> SqlResult<Option<Vec<u8>>> {
    if matches!(value, Value::Null) {
        return Ok(None); // NULLs never conflict
    }
    let ck = keys::col_key_of(value).map_err(SqlError::from)?;
    Ok(view.uniques[uidx].get(&ck).cloned())
}

/// First conflicting live row of one incoming row: its own pk first,
/// then unique-index owners in schema order. Deterministic (MySQL's
/// "first found wins" made stable).
fn find_conflict(
    view: &ConflictView,
    schema: &TableSchema,
    incoming: &[Value],
) -> SqlResult<Option<(Vec<u8>, Vec<Value>)>> {
    let pk = pk_key_of(schema, incoming)?;
    if let Some(r) = row_at(view, &pk) {
        return Ok(Some((pk, r.clone())));
    }
    for (uidx, (pos, _)) in unique_indexes(schema).into_iter().enumerate() {
        let Some(owner) = unique_owner(view, uidx, &incoming[pos])? else {
            continue;
        };
        if owner != pk {
            if let Some(r) = row_at(view, &owner) {
                return Ok(Some((owner, r.clone())));
            }
        }
    }
    Ok(None)
}

/// Record one decided write in the overlay (later incoming rows see
/// it) AND in the unique maps, which stay the single source of truth
/// for value ownership: each deleted row releases only the values it
/// still owns, then the put row claims its own values under its
/// (possibly moved) pk. A value freed earlier in the statement is
/// therefore simply absent -- never a stale owner.
fn overlay_write(view: &mut ConflictView, schema: &TableSchema, w: &RowWrite) -> SqlResult<()> {
    let indexes = unique_indexes(schema);
    for (pk, row) in &w.deletes {
        for (m, &(pos, _)) in view.uniques.iter_mut().zip(&indexes) {
            if let Some(ck) = unique_claim(row, pos)? {
                if m.get(&ck) == Some(pk) {
                    m.remove(&ck); // only release our own ownership
                }
            }
        }
        view.overlay.insert(pk.clone(), None);
    }
    if let Some(put) = &w.put {
        let pk = pk_key_of(schema, put)?;
        for (m, &(pos, _)) in view.uniques.iter_mut().zip(&indexes) {
            if let Some(ck) = unique_claim(put, pos)? {
                m.insert(ck, pk.clone());
            }
        }
        view.overlay.insert(pk, Some(put.clone()));
    }
    Ok(())
}

/// Encoded unique key of one row at `pos`; None for NULL (NULLs never
/// conflict, so they hold no ownership to release or claim).
fn unique_claim(row: &[Value], pos: usize) -> SqlResult<Option<Vec<u8>>> {
    match &row[pos] {
        Value::Null => Ok(None),
        v => keys::col_key_of(v).map(Some).map_err(SqlError::from),
    }
}

/// `INSERT ... ON DUPLICATE KEY UPDATE`: per incoming row, no conflict
/// inserts; a conflict applies the assignments to the EXISTING row
/// with `VALUES(col)` bound to the incoming row.
pub(crate) async fn run_odku(
    shared: &Shared,
    sess: &mut SqlSession,
    schema: &TableSchema,
    incoming: Vec<Vec<Value>>,
    assignments: &[(String, Expr)],
) -> SqlResult<ExecOutcome> {
    // Assignment targets resolve once, before any write (ER 1054).
    for (col, _) in assignments {
        schema.column_index(col).ok_or_else(|| bad_field(col))?;
    }
    let scope = single_table_scope(schema);
    let mut view = conflict_view(shared, schema, sess.txn.as_ref())?;
    let read_ts = view.read_ts;
    let mut writes: Vec<RowWrite> = Vec::with_capacity(incoming.len());
    let mut affected = 0u64;
    for row in incoming {
        let Some((cpk, crow)) = find_conflict(&view, schema, &row)? else {
            writes.push(RowWrite {
                deletes: Vec::new(),
                put: Some(row),
            });
            overlay_write(&mut view, schema, writes.last().expect("just pushed"))?;
            affected += 1;
            continue;
        };
        // UPDATE branch: VALUES(col) <- incoming row's value; plain
        // column refs keep reading the existing row.
        let assigns = assignments
            .iter()
            .map(|(c, e)| Ok((c.clone(), subst_values(e, schema, &row)?)))
            .collect::<SqlResult<Vec<_>>>()?;
        let new = apply_assignments(schema, &scope, &crow, &assigns)?;
        if new == crow {
            // identical after the update: no write, affected 0 (MySQL)
            continue;
        }
        // A pk move must not land on another live/overlaid row
        // (MySQL ER 1062) -- silently overwriting it would lose data.
        let new_pk = pk_key_of(schema, &new)?;
        if new_pk != cpk && row_at(&view, &new_pk).is_some() {
            return Err(pk_dup_entry(schema, &new));
        }
        // Unique-value preemption, same decide-time rejection: the
        // updated row's new UNIQUE values must not be owned by a
        // DIFFERENT live row (MySQL ER 1062 at the statement, not one
        // deferred to COMMIT or whitewashed away by the batch
        // `vacated` set). `cpk` is the row being updated -- its claims
        // are released by this very write, so an owner of cpk is not
        // a preemption; `new_pk` self-ownership only arises when the
        // pk moved, and any other resident at `new_pk` was already
        // rejected above.
        for (uidx, (pos, idx)) in unique_indexes(schema).into_iter().enumerate() {
            if let Some(owner) = unique_owner(&view, uidx, &new[pos])? {
                if owner != cpk && owner != new_pk {
                    return Err(dup_entry(&new[pos], &idx.name));
                }
            }
        }
        writes.push(RowWrite {
            deletes: vec![(cpk, crow)],
            put: Some(new),
        });
        overlay_write(&mut view, schema, writes.last().expect("just pushed"))?;
        affected += 2;
    }
    apply_writes(shared, sess, schema, writes, read_ts, affected).await
}

/// `REPLACE INTO`: per incoming row, delete EVERY conflicting row (pk
/// + all unique hits, deduped), then insert; affected = deleted + 1.
pub(crate) async fn run_replace(
    shared: &Shared,
    sess: &mut SqlSession,
    schema: &TableSchema,
    incoming: Vec<Vec<Value>>,
) -> SqlResult<ExecOutcome> {
    let mut view = conflict_view(shared, schema, sess.txn.as_ref())?;
    let read_ts = view.read_ts;
    let mut writes: Vec<RowWrite> = Vec::with_capacity(incoming.len());
    let mut affected = 0u64;
    for row in incoming {
        let pk = pk_key_of(schema, &row)?;
        let mut hits: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
        if let Some(r) = row_at(&view, &pk) {
            hits.push((pk.clone(), r.clone()));
        }
        for (uidx, (pos, _)) in unique_indexes(schema).into_iter().enumerate() {
            let Some(owner) = unique_owner(&view, uidx, &row[pos])? else {
                continue;
            };
            if owner == pk || hits.iter().any(|(hpk, _)| *hpk == owner) {
                continue;
            }
            if let Some(r) = row_at(&view, &owner) {
                hits.push((owner, r.clone()));
            }
        }
        affected += hits.len() as u64 + 1;
        writes.push(RowWrite {
            deletes: hits,
            put: Some(row),
        });
        overlay_write(&mut view, schema, writes.last().expect("just pushed"))?;
    }
    apply_writes(shared, sess, schema, writes, read_ts, affected).await
}

/// Reject a stray `VALUES()` marker reaching the generic evaluator
/// (it must have been substituted by [`subst_values`] first).
pub(crate) fn stray_values_marker(col: &str) -> SqlError {
    SqlError::new(
        ErrorCode::NotSupported,
        format!("VALUES({col}) is only valid in ON DUPLICATE KEY UPDATE"),
    )
}

#[cfg(test)]
#[path = "upsert_tests.rs"]
mod tests;
