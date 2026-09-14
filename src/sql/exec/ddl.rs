//! CREATE/DROP TABLE and CREATE/DROP INDEX.
//!
//! DDL is linearizable through the raft control plane: each statement
//! runs its whole critical section -- schema reads (lookup, table-id
//! allocation, index pre-checks) AND the catalog write -- inside ONE
//! `shared.raft.write()` guard window on the blocking pool
//! (`catalog_txn` below). That guard is the DDL mutex: two concurrent
//! CREATEs can never both observe the same max table id, and two CREATE
//! INDEX statements can never both pass the same existence check.
//! Because the `CatalogTxn` borrows the guard across its awaits, the
//! window must stay off the async executors (the futures the MySQL shim
//! polls must stay `Send`), hence `spawn_blocking`.
//!
//! Physical rows of a dropped row-engine table are intentionally left
//! orphaned: the catalog tombstone makes them unreachable, and a
//! recreated table gets a fresh id, so orphans never alias a new table.
//! Columnar tables are different: their segment files would accumulate
//! forever, so DROP also purges the 0x23 metas, the registry entries
//! and the files (`columnar::commit::drop_table_segments`) once the
//! catalog drop has landed.
//!
//! Follow-up work (AUTO_INCREMENT counter lifecycle, index entry
//! backfill/sweep, columnar segment purge) runs AFTER the window: it
//! needs the async executors and its correctness does not depend on
//! serializing against other DDL.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::dist;
use crate::sql::exec::scan;
use crate::sql::exec::ExecOutcome;
use crate::sql::index::{self, IndexOps, IndexRef};
use crate::sql::parse::ast::{ColumnSpec, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog::{self, CatalogTxn};
use crate::sql::storage::replicate;
use crate::sql::storage::row;
use crate::sql::storage::schema::{ColumnDef, Engine, IndexDef, KeyModel, SqlType, TableSchema};
use crate::state::{RaftState, Shared};
use crate::store::ops;

pub async fn run(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::CreateTable {
            name,
            if_not_exists,
            columns,
            pk,
            engine,
            starrocks,
        } => {
            create_table(
                shared,
                &name,
                if_not_exists,
                &columns,
                &pk,
                engine,
                starrocks.as_ref(),
            )
            .await
        }
        Statement::DropTable { name, if_exists } => drop_table(shared, &name, if_exists).await,
        Statement::CreateIndex {
            table,
            name,
            column,
            unique,
            if_not_exists,
        } => create_index(shared, &table, &name, &column, unique, if_not_exists).await,
        Statement::DropIndex {
            table,
            name,
            if_exists,
        } => drop_index(shared, &table, &name, if_exists).await,
        _ => unreachable!("dispatch maps only DDL statements here"),
    }
}

/// One catalog mutation to apply under the DDL lock. Drop carries the
/// schema: its id becomes the tombstone value (monotone id allocation).
/// Kv carries a raw FSM entry (the AUTO_INCREMENT counter lifecycle).
enum CatalogMutation {
    Put(TableSchema),
    Drop(TableSchema),
    Kv { key: String, value: String },
}

/// The decision one DDL critical section arrived at: mutations to apply
/// under the held raft write guard, plus the schema the caller's
/// follow-up work continues from (echoed because the lookup ran INSIDE
/// the guard). `changed` distinguishes "the window decided nothing"
/// (IF NOT/EXISTS no-ops) from a real decision: `catalog_txn` drains
/// `mutations` as it applies them, so an applied plan comes back with
/// an empty `mutations` and that emptiness must not be read as a no-op.
struct DdlPlan {
    mutations: Vec<CatalogMutation>,
    schema: Option<TableSchema>,
    changed: bool,
}

impl DdlPlan {
    fn noop() -> DdlPlan {
        DdlPlan {
            mutations: Vec::new(),
            schema: None,
            changed: false,
        }
    }
}

/// Run `begin` + the txn method while holding the raft write guard, on
/// the blocking pool (`CatalogTxn`'s guard borrow spans its await).
/// Kept for single-mutation follow-ups that need no decision (the
/// AUTO_INCREMENT counter lifecycle).
async fn catalog_apply(shared: &Shared, mutation: CatalogMutation) -> SqlResult<()> {
    let raft = Arc::clone(&shared.raft);
    let handle = tokio::runtime::Handle::current();
    let applied = tokio::task::spawn_blocking(move || {
        let mut guard = raft.write().unwrap();
        let mut txn: CatalogTxn<'_> = catalog::begin(&mut guard, "DDL").map_err(SqlError::from)?;
        match mutation {
            CatalogMutation::Put(schema) => handle.block_on(txn.put(&schema)),
            CatalogMutation::Drop(schema) => handle.block_on(txn.drop(&schema.name, schema.id)),
            CatalogMutation::Kv { key, value } => handle.block_on(txn.put_kv(&key, &value)),
        }
        .map_err(SqlError::from)?;
        Ok::<_, SqlError>(txn.applied().to_vec())
    })
    .await
    .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))??;
    // The ack implies follower visibility: hold the response until the
    // peers' FSMs serve the mutation (best-effort, see `replicate`).
    replicate::wait_peers_serve(shared, &applied).await;
    Ok(())
}

/// Run `decide` + its mutations inside ONE raft write-guard window on
/// the blocking pool: schema reads (lookup / id allocation / index
/// pre-checks) and the catalog write are atomic, so two concurrent
/// CREATEs can never observe the same max table id and two CREATE INDEX
/// statements can never both pass the same existence check. `decide`
/// sees the FSM view through the held guard and must only do cheap
/// reads (plus local store reads for the unique-index pre-check).
async fn catalog_txn<F>(shared: &Shared, decide: F) -> SqlResult<DdlPlan>
where
    F: FnOnce(&RaftState) -> SqlResult<DdlPlan> + Send + 'static,
{
    let raft = Arc::clone(&shared.raft);
    let handle = tokio::runtime::Handle::current();
    let (plan, applied) = tokio::task::spawn_blocking(move || {
        let mut guard = raft.write().unwrap();
        // begin() keeps the leadership check FIRST (a follower must get
        // the "requires the raft leader" error, not a decision error).
        let mut txn: CatalogTxn<'_> = catalog::begin(&mut guard, "DDL").map_err(SqlError::from)?;
        let mut plan = decide(txn.state())?;
        for mutation in std::mem::take(&mut plan.mutations) {
            match mutation {
                CatalogMutation::Put(schema) => handle.block_on(txn.put(&schema)),
                CatalogMutation::Drop(schema) => handle.block_on(txn.drop(&schema.name, schema.id)),
                CatalogMutation::Kv { key, value } => handle.block_on(txn.put_kv(&key, &value)),
            }
            .map_err(SqlError::from)?;
        }
        Ok::<_, SqlError>((plan, txn.applied().to_vec()))
    })
    .await
    .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))??;
    // The ack implies follower visibility: hold the response until the
    // peers' FSMs serve the mutation (best-effort, see `replicate`).
    replicate::wait_peers_serve(shared, &applied).await;
    Ok(plan)
}

async fn create_table(
    shared: &Shared,
    name: &str,
    if_not_exists: bool,
    columns: &[ColumnSpec],
    pk: &str,
    engine: Engine,
    starrocks: Option<&crate::sql::parse::starrocks::StarRocksModel>,
) -> SqlResult<ExecOutcome> {
    let schema = build_schema(0, name, columns, pk, engine, starrocks)?;
    let table = name.to_string();
    let plan = catalog_txn(shared, move |raft| {
        if catalog::lookup_state(raft, &table)?.is_some() {
            if if_not_exists {
                return Ok(DdlPlan::noop());
            }
            return Err(SqlError::new(
                ErrorCode::TableExists,
                format!("table '{table}' already exists"),
            ));
        }
        let mut schema = schema;
        schema.id = alloc_table_id(raft);
        Ok(DdlPlan {
            mutations: vec![CatalogMutation::Put(schema.clone())],
            schema: Some(schema),
            changed: true,
        })
    })
    .await?;
    // IF NOT EXISTS on an existing table: the window decided nothing.
    let Some(schema) = plan.schema else {
        return Ok(ExecOutcome::Ok);
    };
    // Counter lifecycle rides the same replicated path: CREATE seeds
    // `sql_sequence/<table>` = 1 alongside the schema (lazy default 1
    // also covers it, but an explicit entry makes cluster state visible
    // and keeps the invariant "flag set => counter exists").
    if schema.auto_increment.is_some() {
        catalog_apply(
            shared,
            CatalogMutation::Kv {
                key: catalog::sequence_key(&schema.name),
                value: "1".to_string(),
            },
        )
        .await?;
    }
    Ok(ExecOutcome::Ok)
}

async fn drop_table(shared: &Shared, name: &str, if_exists: bool) -> SqlResult<ExecOutcome> {
    let table = name.to_string();
    let plan = catalog_txn(shared, move |raft| {
        let Some(schema) = catalog::lookup_state(raft, &table)? else {
            if if_exists {
                return Ok(DdlPlan::noop());
            }
            return Err(SqlError::no_such_table(&table));
        };
        Ok(DdlPlan {
            mutations: vec![CatalogMutation::Drop(schema.clone())],
            schema: Some(schema),
            changed: true,
        })
    })
    .await?;
    // IF EXISTS on a missing table: the window decided nothing.
    let Some(schema) = plan.schema else {
        return Ok(ExecOutcome::Ok);
    };
    // Clear the AUTO_INCREMENT counter so a recreated table starts at 1
    // again ("" is the house tombstone: reads fall back to the default).
    if schema.auto_increment.is_some() {
        catalog_apply(
            shared,
            CatalogMutation::Kv {
                key: catalog::sequence_key(&schema.name),
                value: String::new(),
            },
        )
        .await?;
    }
    if schema.engine.is_columnar() {
        crate::sql::columnar::commit::drop_table_segments(shared, schema.id).await?;
    }
    Ok(ExecOutcome::Ok)
}

async fn create_index(
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

/// Write index entries for every live row (leader-side, after the
/// catalog entry committed). One synced batch per whole backfill.
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
        let pk_key = row::pk_encode(&r[schema.pk_index()]).map_err(SqlError::from)?;
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

async fn drop_index(
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

fn lookup_table(shared: &Shared, table: &str) -> SqlResult<TableSchema> {
    catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))
}

/// Next free table id: max of the live and tombstoned ids plus one, so
/// ids stay monotone across drop+recreate cycles even after restarts.
/// Runs inside the DDL guard window (`catalog_txn`), which is what
/// keeps two concurrent CREATEs from observing the same max.
fn alloc_table_id(raft: &RaftState) -> u32 {
    let live_max = catalog::list_tables_state(raft)
        .iter()
        .map(|s| s.id)
        .max()
        .unwrap_or(0);
    let dropped_max = catalog::dropped_ids_state(raft)
        .into_iter()
        .max()
        .unwrap_or(0);
    live_max.max(dropped_max) + 1
}

/// Validate a CREATE TABLE body and build its schema (id supplied by
/// the caller: 0 while validating, the allocated id before the put).
pub fn build_schema(
    id: u32,
    name: &str,
    columns: &[ColumnSpec],
    pk: &str,
    engine: Engine,
    starrocks: Option<&crate::sql::parse::starrocks::StarRocksModel>,
) -> SqlResult<TableSchema> {
    // StarRocks table models (Phase 3): DUPLICATE implies the
    // append-only columnar engine; PRIMARY KEY is the row-store upsert
    // model and cannot ride the columnar engine.
    let key_model = starrocks.map(|m| m.kind).unwrap_or(KeyModel::MySql);
    let engine = match (key_model, engine) {
        (KeyModel::Duplicate, _) => Engine::Columnar,
        (KeyModel::PrimaryKey, Engine::Columnar) => {
            return Err(SqlError::new(
                ErrorCode::NotSupported,
                "StarRocks PRIMARY KEY tables are row-store upsert tables \
                 (ENGINE=columnar is not supported)",
            ))
        }
        (_, e) => e,
    };
    // DISTRIBUTED BY columns must exist; buckets must be positive.
    // (Distribution is recorded metadata -- placement stays crc16.)
    if let Some(d) = starrocks.and_then(|m| m.distribution.as_ref()) {
        if d.buckets == 0 {
            return Err(SqlError::new(
                ErrorCode::Parse,
                "BUCKETS must be at least 1",
            ));
        }
        for c in &d.columns {
            if !columns.iter().any(|col| col.name.eq_ignore_ascii_case(c)) {
                return Err(SqlError::new(
                    ErrorCode::BadField,
                    format!("unknown column '{c}' in DISTRIBUTED BY"),
                ));
            }
        }
    }
    let pk_idx = columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(pk))
        .ok_or_else(|| {
            SqlError::new(
                ErrorCode::Parse,
                format!("primary key column '{pk}' not found"),
            )
        })?;
    // AUTO_INCREMENT validation (MySQL 1075/1063): at most one auto
    // column, integer type (TINYINT/SMALLINT/INT/BIGINT all translate
    // to SqlType::Int; BOOL is a distinct engine type and rejected),
    // and the column must be the primary key -- the only key the
    // engine supports, so MySQL's "must be defined as a key" narrows
    // to "must be THE pk".
    let auto_cols: Vec<&ColumnSpec> = columns.iter().filter(|c| c.auto_increment).collect();
    let auto_increment = match auto_cols.as_slice() {
        [] => None,
        [one] => {
            if one.sql_type != SqlType::Int {
                return Err(SqlError::new(
                    ErrorCode::WrongAutoKey,
                    format!(
                        "Incorrect column specifier for column '{}'; AUTO_INCREMENT \
                         requires an integer column (TINYINT/SMALLINT/INT/BIGINT)",
                        one.name
                    ),
                ));
            }
            if !one.name.eq_ignore_ascii_case(pk) {
                return Err(SqlError::new(
                    ErrorCode::WrongAutoKey,
                    "Incorrect table definition; there can be only one auto column \
                     and it must be defined as a key"
                        .to_string(),
                ));
            }
            Some(one.name.clone())
        }
        many => {
            let _ = many;
            return Err(SqlError::new(
                ErrorCode::WrongAutoKey,
                "Incorrect table definition; there can be only one auto column \
                 and it must be defined as a key"
                    .to_string(),
            ));
        }
    };
    let mut defs = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        if defs
            .iter()
            .any(|d: &ColumnDef| d.name.eq_ignore_ascii_case(&c.name))
        {
            return Err(SqlError::new(
                ErrorCode::Parse,
                format!("duplicate column '{}'", c.name),
            ));
        }
        // A primary key is implicitly NOT NULL (MySQL semantics), even
        // if the body said NULL. DUPLICATE-model tables keep the
        // declared nullability: their "pk" is recorded metadata of the
        // first dup-key column, not a dedup key.
        let pk_not_null = i == pk_idx && key_model != KeyModel::Duplicate;
        defs.push(ColumnDef {
            name: c.name.clone(),
            sql_type: c.sql_type,
            nullable: c.nullable && !pk_not_null,
        });
    }
    Ok(TableSchema {
        id,
        name: name.to_string(),
        columns: defs,
        pk: columns[pk_idx].name.clone(),
        auto_increment,
        engine,
        indexes: Vec::new(),
        key_model,
        distribution: starrocks.and_then(|m| m.distribution.clone()),
    })
}

#[cfg(test)]
#[path = "ddl_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ddl_starrocks_tests.rs"]
mod starrocks_tests;
