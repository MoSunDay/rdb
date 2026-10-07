//! CREATE/DROP TABLE (the ALTER family -- CREATE/DROP INDEX,
//! TRUNCATE, RENAME -- lives in `ddl_alter`, riding the same
//! machinery).
//!
//! DDL is linearizable through the raft control plane: each statement
//! runs its whole critical section -- schema reads (lookup, table-id
//! allocation, index pre-checks) AND the catalog write -- inside ONE
//! `shared.raft.write()` guard window on the blocking pool
//! (`catalog_txn` below). That guard is the DDL mutex: two concurrent
//! CREATEs can never both observe the same max table id, and two CREATE
//! INDEX statements can never both pass the same existence check.
//! Because the `CatalogTxn` borrows the guard, the window must stay
//! off the async executors (the futures the MySQL shim polls must stay
//! `Send`), hence `spawn_blocking`. The guard is released before any
//! commit await: holding it across `raft_apply_await` deadlocks the
//! leader (see `catalog_txn`).
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

use crate::sql::exec::ddl_alter;
use crate::sql::exec::show;
use crate::sql::exec::ExecOutcome;
use crate::sql::parse::ast::{ColumnSpec, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog::{self, CatalogTxn};
use crate::sql::storage::replicate;
use crate::sql::storage::schema::{ColumnDef, Engine, KeyModel, SqlType, TableSchema};
use crate::state::{self, RaftState, Shared};

pub async fn run(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::CreateTable {
            name,
            if_not_exists,
            columns,
            pk,
            engine,
            starrocks,
            indexes,
        } => {
            create_table(
                shared,
                &CreateTableBody {
                    name: &name,
                    if_not_exists,
                    columns: &columns,
                    pk: &pk,
                    engine,
                    starrocks: starrocks.as_ref(),
                    indexes: &indexes,
                },
            )
            .await
        }
        Statement::DropTable { name, if_exists } => drop_table(shared, &name, if_exists).await,
        // The ALTER family (index create/drop, TRUNCATE, RENAME) lives
        // in `ddl_alter` but rides the SAME machinery below.
        Statement::CreateIndex {
            table,
            name,
            column,
            unique,
            if_not_exists,
        } => ddl_alter::create_index(shared, &table, &name, &column, unique, if_not_exists).await,
        Statement::DropIndex {
            table,
            name,
            if_exists,
        } => ddl_alter::drop_index(shared, &table, &name, if_exists).await,
        Statement::TruncateTable { name } => ddl_alter::truncate_table(shared, &name).await,
        Statement::RenameTable { from, to } => ddl_alter::rename_table(shared, &from, &to).await,
        _ => unreachable!("dispatch maps only DDL statements here"),
    }
}

/// One catalog mutation to apply under the DDL lock. Drop carries the
/// schema: it queues an id-less name tombstone PLUS a
/// `sql_dropped/<id>` marker -- the side entry is what retires the id
/// (monotone allocation, MVCC GC); the name tombstone alone (RENAME's
/// shape) keeps the id live. Kv carries a raw FSM entry (the
/// AUTO_INCREMENT counter lifecycle).
pub(crate) enum CatalogMutation {
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
pub(crate) struct DdlPlan {
    pub(crate) mutations: Vec<CatalogMutation>,
    pub(crate) schema: Option<TableSchema>,
    pub(crate) changed: bool,
}

impl DdlPlan {
    pub(crate) fn noop() -> DdlPlan {
        DdlPlan {
            mutations: Vec::new(),
            schema: None,
            changed: false,
        }
    }
}

/// Serializes whole DDLs (decide -> queue -> commit-await) without
/// holding the raft write guard across any await: the second CREATE
/// must not run its existence check before the first one's commit is
/// visible in the FSM's `live_kv` (the original design got this
/// ordering by blocking on the raft guard through the commit; that
/// guard-across-await starved the leader's raft/HTTP serve paths and
/// ts refill -- a proven 4-core hang, see `catalog_txn`). It also
/// serializes `exec::sequence::allocate`'s floor read-modify-write
/// (read floor -> queue bump -> commit-await) so two concurrent
/// INSERTs can never observe the same floor while a bump is
/// queued-but-unapplied.
pub(crate) static CATALOG_MUX: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Run `begin` + queue the single mutation inside ONE raft write-guard
/// window on the blocking pool, then await the commit AFTER the guard
/// is dropped: the guard must never span an await (apply progress, ts
/// refill and the raft/HTTP serve paths all take the same RwLock;
/// holding it across `raft_apply_await` deadlocks the leader). Kept
/// for single-mutation follow-ups that need no decision (the
/// AUTO_INCREMENT counter lifecycle).
pub(crate) async fn catalog_apply(shared: &Shared, mutation: CatalogMutation) -> SqlResult<()> {
    let _catalog = CATALOG_MUX.lock().await;
    let raft = Arc::clone(&shared.raft);
    let queued = tokio::task::spawn_blocking(move || {
        let mut guard = raft.write().unwrap();
        let mut txn: CatalogTxn<'_> = catalog::begin(&mut guard, "DDL").map_err(SqlError::from)?;
        let mut queued = Vec::new();
        match mutation {
            CatalogMutation::Put(schema) => {
                queued.push(txn.queue_put(&schema).map_err(SqlError::from)?)
            }
            // Drop queues two entries (name tombstone + sql_dropped
            // marker): both are awaited and replicated below.
            CatalogMutation::Drop(schema) => queued.extend(
                txn.queue_drop(&schema.name, schema.id)
                    .map_err(SqlError::from)?,
            ),
            CatalogMutation::Kv { key, value } => {
                queued.push(txn.queue_put_kv(&key, &value).map_err(SqlError::from)?)
            }
        }
        Ok::<_, SqlError>(queued)
    })
    .await
    .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))??;
    let mut applied = Vec::with_capacity(queued.len());
    for q in queued {
        state::raft_apply_await(q.ticket)
            .await
            .map_err(SqlError::from)?;
        applied.push((q.key, q.value));
    }
    // The ack implies follower visibility: hold the response until the
    // peers' FSMs serve the mutation (best-effort, see `replicate`).
    replicate::wait_peers_serve(shared, &applied).await;
    Ok(())
}

/// Run `decide` + queue its mutations inside ONE raft write-guard
/// window on the blocking pool: schema reads (lookup / id allocation /
/// index pre-checks) and the queued catalog writes are atomic, so two
/// concurrent CREATEs can never observe the same max table id and two
/// CREATE INDEX statements can never both pass the same existence
/// check. `decide` sees the FSM view through the held guard and must
/// only do cheap reads (plus local store reads for the unique-index
/// pre-check). The commits are awaited AFTER the guard is dropped: the
/// guard must never span an await (see `catalog_apply`).
pub(crate) async fn catalog_txn<F>(shared: &Shared, decide: F) -> SqlResult<DdlPlan>
where
    F: FnOnce(&RaftState) -> SqlResult<DdlPlan> + Send + 'static,
{
    let _catalog = CATALOG_MUX.lock().await;
    let raft = Arc::clone(&shared.raft);
    let (plan, queued) = tokio::task::spawn_blocking(move || {
        let mut guard = raft.write().unwrap();
        // begin() keeps the leadership check FIRST (a follower must get
        // the "requires the raft leader" error, not a decision error).
        let mut txn: CatalogTxn<'_> = catalog::begin(&mut guard, "DDL").map_err(SqlError::from)?;
        let mut plan = decide(txn.state())?;
        let mut queued = Vec::with_capacity(plan.mutations.len());
        for mutation in std::mem::take(&mut plan.mutations) {
            match mutation {
                CatalogMutation::Put(schema) => {
                    queued.push(txn.queue_put(&schema).map_err(SqlError::from)?)
                }
                // Drop queues two entries (name tombstone +
                // sql_dropped marker): the plan length underestimates
                // the awaited commits, which is fine -- the vec grows.
                CatalogMutation::Drop(schema) => queued.extend(
                    txn.queue_drop(&schema.name, schema.id)
                        .map_err(SqlError::from)?,
                ),
                CatalogMutation::Kv { key, value } => {
                    queued.push(txn.queue_put_kv(&key, &value).map_err(SqlError::from)?)
                }
            }
        }
        Ok::<_, SqlError>((plan, queued))
    })
    .await
    .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))??;
    let mut applied = Vec::with_capacity(queued.len());
    for q in queued {
        state::raft_apply_await(q.ticket)
            .await
            .map_err(SqlError::from)?;
        applied.push((q.key, q.value));
    }
    // The ack implies follower visibility: hold the response until the
    // peers' FSMs serve the mutation (best-effort, see `replicate`).
    replicate::wait_peers_serve(shared, &applied).await;
    Ok(plan)
}

/// The borrowed pieces of one `Statement::CreateTable` (keeps
/// `create_table` under clippy's argument budget).
struct CreateTableBody<'a> {
    name: &'a str,
    if_not_exists: bool,
    columns: &'a [ColumnSpec],
    pk: &'a [String],
    engine: Engine,
    starrocks: Option<&'a crate::sql::parse::starrocks::StarRocksModel>,
    indexes: &'a [crate::sql::parse::ast::InlineIndex],
}

async fn create_table(shared: &Shared, body: &CreateTableBody<'_>) -> SqlResult<ExecOutcome> {
    let (name, if_not_exists, indexes) = (&body.name, body.if_not_exists, body.indexes);
    let schema = build_schema(0, name, body.columns, body.pk, body.engine, body.starrocks)?;
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
    // Inline KEY/UNIQUE KEY constraints become real indexes right after
    // the schema lands: the same windowed path CREATE INDEX takes (the
    // fresh table is empty, so there is nothing to backfill).
    for idx in indexes {
        ddl_alter::create_index(
            shared,
            &schema.name,
            &idx.name,
            &idx.column,
            idx.unique,
            false,
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

pub(crate) fn lookup_table(shared: &Shared, table: &str) -> SqlResult<TableSchema> {
    catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))
}

/// Next free table id: max of the live and tombstoned ids plus one, so
/// ids stay monotone across drop+recreate cycles even after restarts.
/// Runs inside the DDL guard window (`catalog_txn`), which is what
/// keeps two concurrent CREATEs from observing the same max.
pub(crate) fn alloc_table_id(raft: &RaftState) -> u32 {
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
    pk: &[String],
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
    let pk_indices: Vec<usize> = pk
        .iter()
        .map(|p| {
            columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(p))
                .ok_or_else(|| {
                    SqlError::new(
                        ErrorCode::Parse,
                        format!("primary key column '{p}' not found"),
                    )
                })
        })
        .collect::<SqlResult<Vec<_>>>()?;
    // DECIMAL storage/encoding exists (W2.0 batch 1), but the key
    // encodings are not decimal-aware yet, so a DECIMAL pk is a loud
    // 1235 rejection rather than a mis-ordered keyspace.
    if pk_indices.len() == 1 && matches!(columns[pk_indices[0]].sql_type, SqlType::Decimal { .. }) {
        return Err(SqlError::unsupported(
            "DECIMAL primary key is not supported (use an integer pk)",
        ));
    }
    // Composite pk column types are narrowed to the fixed-width and
    // varlen key components that concatenate unambiguously: Bool and
    // Double have no composite key support, Decimal is rejected above
    // for single pks too, and Blob is rejected outright.
    if pk_indices.len() > 1 {
        for &i in &pk_indices {
            let c = &columns[i];
            if !matches!(
                c.sql_type,
                SqlType::Int | SqlType::Date | SqlType::DateTime | SqlType::VarChar
            ) {
                return Err(SqlError::unsupported(format!(
                    "composite PRIMARY KEY column '{}' has type {}; allowed types: \
                     TINYINT/SMALLINT/INT/BIGINT, VARCHAR, DATE, DATETIME",
                    c.name,
                    show::type_name(c.sql_type)
                )));
            }
        }
    }
    // Same guard for the append-only columnar engine: its segment
    // pages have no decimal encoding (see `columnar::encode`).
    if engine.is_columnar()
        && columns
            .iter()
            .any(|c| matches!(c.sql_type, SqlType::Decimal { .. }))
    {
        return Err(SqlError::unsupported(
            "DECIMAL columns are not supported on columnar tables",
        ));
    }
    // AUTO_INCREMENT validation (MySQL 1075/1063): at most one auto
    // column, integer type (TINYINT/SMALLINT/INT/BIGINT all translate
    // to SqlType::Int; BOOL is a distinct engine type and rejected),
    // and the column must be the entire primary key -- the only key
    // the engine supports, so MySQL's "must be defined as a key"
    // narrows to "must be THE pk" (a composite pk containing the auto
    // column therefore rejects).
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
            if pk.len() != 1 || !one.name.eq_ignore_ascii_case(&pk[0]) {
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
        // Every primary-key column is implicitly NOT NULL (MySQL
        // semantics), even if the body said NULL. DUPLICATE-model
        // tables keep the declared nullability: their "pk" is recorded
        // metadata of the first dup-key column, not a dedup key.
        let pk_not_null = key_model != KeyModel::Duplicate && pk_indices.contains(&i);
        defs.push(ColumnDef {
            name: c.name.clone(),
            sql_type: c.sql_type,
            nullable: c.nullable && !pk_not_null,
        });
    }
    // Canonical pk names (the column list's casing, not the
    // constraint's) keep catalog lookups and SHOW output stable.
    let pk = pk_indices
        .into_iter()
        .map(|i| columns[i].name.clone())
        .collect();
    Ok(TableSchema {
        id,
        name: name.to_string(),
        columns: defs,
        pk,
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
