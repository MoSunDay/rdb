//! ALTER-family DDL (M4): CREATE/DROP INDEX (moved here from `ddl`),
//! `TRUNCATE TABLE` and `RENAME TABLE`. Everything rides the same
//! raft-guarded critical section as CREATE/DROP TABLE (`ddl::
//! catalog_txn`): schema reads and catalog writes atomic under the DDL
//! mutex, commits awaited after the guard drops, ack held until
//! followers serve the mutation.
//!
//! ## TRUNCATE: the table-id swap
//!
//! TRUNCATE keeps the schema byte-identical but allocates a FRESH
//! `table_id`, tombstoning the old one: physically it behaves exactly
//! like DROP + re-CREATE of the same definition. That makes the wipe
//! correct everywhere at once, locally and in the cluster:
//! - rows and secondary/unique index entries are keyed by table_id, so
//!   every version of every pk (any slot, any node) becomes unreachable
//!   the moment the swapped catalog entry lands -- the replicated
//!   effect IS the truncate marker, no per-node data RPC needed;
//! - the Drop here queues an id-less name tombstone PLUS the
//!   `sql_dropped/<old id>` side entry. The side entry lives under its
//!   own key, so the same-name Put of the fresh id CANNOT overwrite it
//!   (the pre-fix tombstone-at-the-name-key was erased by that Put and
//!   the old id leaked forever): the MVCC GC (`storage::gc`) deletes
//!   every row version and index entry of a dropped id, on every node,
//!   so space IS reclaimed in the background (same path as DROP
//!   TABLE);
//! - columnar tables additionally get their segments eagerly purged on
//!   the leader here (`columnar::commit::drop_table_segments`); other
//!   nodes' columnar sweep classifies old-id metas as garbage;
//! - the AUTO_INCREMENT counter (keyed by table NAME) resets to 1 in
//!   the same window, so fresh rows start from the beginning.
//!
//! Not rollbackable, and rejected inside an open transaction like every
//! other DDL (decision point 5 of the mysql-gap plan). The accepted
//! race windows are the DROP TABLE ones: a write that resolved the OLD
//! schema before the swap and lands after it writes old-id bytes that
//! stay invisible (orphaned), and a prepared 2PC write of the old id
//! commits onto orphaned keys.
//!
//! ## RENAME: catalog-only
//!
//! `RENAME TABLE a TO b` rewrites the catalog entry under a different
//! name with the SAME table_id: every physical row, index entry and
//! columnar segment stays where it is (zero-copy), and the name-keyed
//! AUTO_INCREMENT counter moves keys (`sql_sequence/a` -> `sql_sequence/
//! b`, raw value copied verbatim so batch reservations survive).
//! Prepared statements keep their parsed table NAMES and re-resolve on
//! every execution, so statements prepared against the old name fail
//! with "table doesn't exist" naturally. The old name is retired with
//! an id-less tombstone ONLY -- no `sql_dropped/` side entry, so the
//! id (and its rows) never enters the MVCC GC's dropped set; emitting
//! a Drop here would have GC gradually delete the renamed table.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::dist;
use crate::sql::exec::scan;
use crate::sql::exec::ExecOutcome;
use crate::sql::index::{self, IndexOps, IndexRef};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::row;
use crate::sql::storage::schema::{IndexDef, TableSchema};
use crate::state::{self, Shared};
use crate::store::ops;

use super::ddl::{alloc_table_id, catalog_txn, lookup_table, CatalogMutation, DdlPlan};

pub(crate) async fn create_index(
    shared: &Shared,
    table: &str,
    name: &str,
    column: &str,
    unique: bool,
    if_not_exists: bool,
) -> SqlResult<ExecOutcome> {
    // Store handle + snapshot ts are captured BEFORE the window: the
    // unique-index pre-check reads local rows inside it.
    let store = Arc::clone(&shared.store);
    let now = shared.sql_ts.now();
    let (table, name, column) = (table.to_string(), name.to_string(), column.to_string());
    let index = IndexRef {
        name: name.clone(),
        column: column.clone(),
        unique,
    };
    let probe = index.clone();
    let target = table.clone();
    let plan = catalog_txn(shared, move |raft| {
        let index = probe;
        let mut schema =
            catalog::lookup_state(raft, &table)?.ok_or_else(|| SqlError::no_such_table(&table))?;
        if schema.engine.is_columnar() {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                format!("indexes are not supported on columnar table '{table}'"),
            ));
        }
        if schema.index(&name).is_some() {
            if if_not_exists {
                return Ok(DdlPlan::noop());
            }
            return Err(SqlError::new(
                ErrorCode::DupEntry,
                format!("index '{name}' already exists"),
            ));
        }
        if schema.column_index(&column).is_none() {
            return Err(SqlError::new(
                ErrorCode::BadField,
                format!("unknown column '{column}' in '{table}'"),
            ));
        }
        // Multi-column indexes never reach here (translate rejects them),
        // but keep the guard local: M2 indexes exactly one column.
        // UNIQUE pre-check runs BEFORE the catalog entry exists, so a clean
        // rejection leaves nothing behind. Rows are read at the CURRENT
        // committed snapshot.
        if unique {
            let rows = scan::visible_rows(&store, &schema, now)?;
            index::maintain::assert_no_duplicates(&schema, &index, &rows)?;
        }
        let id = catalog::next_index_id(&schema);
        schema.indexes.push(IndexDef {
            id,
            name,
            column,
            unique,
        });
        Ok(DdlPlan {
            mutations: vec![CatalogMutation::Put(schema)],
            schema: None,
            changed: true,
        })
    })
    .await?;
    // IF NOT EXISTS on an existing index: the window decided nothing.
    if !plan.changed {
        return Ok(ExecOutcome::Ok);
    }
    // Backfill: rescan AFTER the catalog entry is committed, so every
    // row visible at this point is covered (any writer that started
    // earlier and lands later may miss its entry -- the accepted M2
    // race window; the residual WHERE filter hides stale entries, and
    // missing entries only cost the planner an index that finds fewer
    // pks than exist, which the fallback heuristic bounds).
    let schema = lookup_table(shared, &target)?;
    backfill_index(shared, &schema, &index).await?;
    Ok(ExecOutcome::Ok)
}

/// Backfill one new index's entries for every LIVE row of the table.
/// The row set is gathered across slot owners (rows live on every
/// node), and the produced entries are routed to the OWNER of each
/// key's slot: a local batch would leave unique-key entries on the
/// leader for slots other nodes own, and the owning participants would
/// then never see them -- the unique veto would miss exactly the
/// pre-existing values. All-local key sets keep the single-batch path.
async fn backfill_index(shared: &Shared, schema: &TableSchema, index: &IndexRef) -> SqlResult<()> {
    let read_ts = shared.sql_ts.now();
    let rows = match dist::gather::gatherable_by_name(shared) {
        Some(bs) => dist::gather::gather_rows(shared, &bs, schema, read_ts).await?,
        None => scan::visible_rows(&shared.store, schema, read_ts)?,
    };
    let mut ops: IndexOps = Vec::with_capacity(rows.len());
    for r in &rows {
        let pk_key = row::pk_encode_row(schema, r).map_err(SqlError::from)?;
        ops.extend(index::entries_for_live_row(schema, index, &pk_key, r).map_err(SqlError::from)?);
    }
    if ops.is_empty() {
        return Ok(());
    }
    // A unique index over already-duplicated values is unbuildable:
    // two pks claiming one key can never both win, and routing them to
    // different owners would silently weaken the constraint. Reject the
    // CREATE like a duplicate insert instead (before any key lands).
    let mut owners: std::collections::BTreeMap<&[u8], &[u8]> = std::collections::BTreeMap::new();
    for (key, val) in &ops {
        let Some(pk) = val.as_deref() else { continue };
        if let Some(prev) = owners.get(key.as_slice()) {
            if *prev != pk {
                return Err(SqlError::new(
                    ErrorCode::DupEntry,
                    format!(
                        "Duplicate entry '{}' for key '{}': the column already holds duplicates",
                        String::from_utf8_lossy(pk),
                        index.name
                    ),
                ));
            }
        }
        owners.insert(key.as_slice(), pk);
    }
    drop(owners);
    // Route entries to each key's slot owner: 2PC when any owner is
    // another node, one local batch otherwise (single-node world or
    // every key on this node).
    let no_writes: dist::plan::SimpleWrites = Vec::new();
    // Reserve the write frontier first like every other commit path:
    // the index-only plan still allocates one ts (the 2PC txn id), and
    // it must clear `read_ts` without degrading to the GAP fallback.
    // Strict when any entry key has a remote owner (2PC backfill).
    let probes: Vec<Vec<u8>> = ops.iter().map(|(k, _)| k.clone()).collect();
    let strict = dist::any_remote_owner(shared, &probes);
    shared
        .sql_ts
        .reserve_write_frontier(read_ts, 1, strict)
        .await?;
    if let Some(plan) = dist::plan::try_plan_simple(shared, read_ts, schema, &no_writes, &ops)? {
        return dist::twopc::run(shared, &plan).await;
    }
    let mut batch = WriteBatch::default();
    index::maintain::apply_ops(&mut batch, ops);
    ops::batch_write_async(Arc::clone(&shared.store), batch)
        .await
        .map_err(SqlError::from)
}

pub(crate) async fn drop_index(
    shared: &Shared,
    table: &str,
    name: &str,
    if_exists: bool,
) -> SqlResult<ExecOutcome> {
    let (table, needle) = (table.to_string(), name.to_string());
    let plan = catalog_txn(shared, move |raft| {
        let name = needle;
        let mut schema =
            catalog::lookup_state(raft, &table)?.ok_or_else(|| SqlError::no_such_table(&table))?;
        let Some(pos) = schema
            .indexes
            .iter()
            .position(|i| i.name.eq_ignore_ascii_case(&name))
        else {
            if if_exists {
                return Ok(DdlPlan::noop());
            }
            return Err(SqlError::new(
                ErrorCode::Unknown,
                format!("index '{name}' doesn't exist"),
            ));
        };
        // Capture the column before the definition leaves the schema: the
        // on-disk keys are identified by (table_id, col_pos) alone. The
        // check runs inside the window so a clean rejection leaves the
        // catalog untouched.
        let col = schema.indexes[pos].column.clone();
        if schema.column_index(&col).is_none() {
            return Err(SqlError::new(
                ErrorCode::BadField,
                format!("unknown column '{col}'"),
            ));
        }
        // Removal by position keeps the remaining index ids stable. The
        // catalog entry goes first: if the entry sweep then fails, the
        // orphaned keys are unreachable (no index def) and harmless, while
        // the reverse order could leave a DECLARED index with no entries.
        // The echoed schema is the PRE-removal snapshot: the follow-up
        // sweep still needs the dropped column's position.
        let echo = schema.clone();
        schema.indexes.remove(pos);
        Ok(DdlPlan {
            mutations: vec![CatalogMutation::Put(schema)],
            schema: Some(echo),
            changed: true,
        })
    })
    .await?;
    // IF EXISTS on a missing index: the window decided nothing.
    if !plan.changed {
        return Ok(ExecOutcome::Ok);
    }
    let Some(snapshot) = plan.schema else {
        return Ok(ExecOutcome::Ok);
    };
    // Recover (table_id, col_pos) from the echoed pre-removal schema;
    // both lookups were validated inside the window.
    let pos = snapshot
        .indexes
        .iter()
        .position(|i| i.name.eq_ignore_ascii_case(name))
        .expect("dropped index present in the pre-removal snapshot");
    let col_pos = snapshot
        .column_index(&snapshot.indexes[pos].column)
        .expect("dropped index column present in the pre-removal snapshot");
    index::drop_entries(Arc::clone(&shared.store), snapshot.id, col_pos as u32)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Ok)
}

/// `TRUNCATE TABLE t`: swap the table id (see the module doc), reset
/// the AUTO_INCREMENT counter, purge columnar segments of the old id.
pub(crate) async fn truncate_table(shared: &Shared, name: &str) -> SqlResult<ExecOutcome> {
    let table = name.to_string();
    // `plan.schema` echoes the PRE-swap schema: the old table_id is
    // what the columnar segments are keyed by.
    let plan = catalog_txn(shared, move |raft| {
        let mut schema =
            catalog::lookup_state(raft, &table)?.ok_or_else(|| SqlError::no_such_table(&table))?;
        let old = schema.clone();
        // Same definition, fresh id: Drop queues the id-less name
        // tombstone PLUS the `sql_dropped/<old id>` marker -- the side
        // entry is what makes every physical key of the old id
        // unreachable-and-collectable, and it lives under its own key,
        // so the same-name Put below cannot erase it (GC reclaims the
        // old id's bytes in the background).
        schema.id = alloc_table_id(raft);
        let mut mutations = vec![
            CatalogMutation::Drop(old.clone()),
            CatalogMutation::Put(schema),
        ];
        if old.auto_increment.is_some() {
            mutations.push(CatalogMutation::Kv {
                key: catalog::sequence_key(&table),
                value: "1".to_string(),
            });
        }
        Ok(DdlPlan {
            mutations,
            schema: Some(old),
            changed: true,
        })
    })
    .await?;
    let Some(old) = plan.schema else {
        return Ok(ExecOutcome::Ok);
    };
    if old.engine.is_columnar() {
        crate::sql::columnar::commit::drop_table_segments(shared, old.id).await?;
    }
    Ok(ExecOutcome::Ok)
}

/// `RENAME TABLE a TO b`: catalog swap at a new name, same table_id,
/// sequence counter key moved verbatim (batch reservations survive).
pub(crate) async fn rename_table(shared: &Shared, from: &str, to: &str) -> SqlResult<ExecOutcome> {
    let (from, to) = (from.to_string(), to.to_string());
    catalog_txn(shared, move |raft| {
        let mut schema =
            catalog::lookup_state(raft, &from)?.ok_or_else(|| SqlError::no_such_table(&from))?;
        // Target must be free: case-insensitive over the live tables
        // (identifier lookups are case-insensitive engine-wide); a
        // case-only rename of the same table is allowed.
        if let Some(clash) = catalog::list_tables_state(raft)
            .into_iter()
            .find(|s| s.name.eq_ignore_ascii_case(&to) && !s.name.eq_ignore_ascii_case(&from))
        {
            return Err(SqlError::new(
                ErrorCode::TableExists,
                format!("table '{}' already exists", clash.name),
            ));
        }
        // Counter moves by raw value: reservations (persisted batches
        // ahead of the handed-out ids) must not regress on rename.
        let old_seq = state::raft_get(raft, &catalog::sequence_key(&from));
        let old_name = schema.name.clone();
        schema.name = to.clone();
        // RENAME must NOT emit `CatalogMutation::Drop`: the id stays
        // live under the new name, and a Drop would ALSO queue the
        // `sql_dropped/<id>` marker, sending the MVCC GC after every
        // row of the renamed table. The old name only needs the
        // id-less tombstone (empty value: readers treat it as absent);
        // ids retire solely via the side entry on a real DROP/TRUNCATE.
        let mut mutations = vec![
            CatalogMutation::Kv {
                key: catalog::catalog_key(&old_name),
                value: String::new(),
            },
            CatalogMutation::Put(schema),
        ];
        if !old_seq.is_empty() {
            mutations.push(CatalogMutation::Kv {
                key: catalog::sequence_key(&to),
                value: old_seq,
            });
        }
        mutations.push(CatalogMutation::Kv {
            key: catalog::sequence_key(&old_name),
            value: String::new(),
        });
        Ok(DdlPlan {
            mutations,
            schema: None,
            changed: true,
        })
    })
    .await?;
    Ok(ExecOutcome::Ok)
}

#[cfg(test)]
#[path = "ddl_alter_tests.rs"]
mod tests;
