//! CREATE/DROP TABLE and CREATE/DROP INDEX.
//!
//! DDL is linearizable through the raft control plane. Schema reads
//! (lookup / id allocation) run first WITHOUT the write lock; then the
//! mutation holds `shared.raft.write()` across `catalog::begin` + the
//! txn method -- that guard is the DDL mutex serializing concurrent
//! CREATEs (which could otherwise both observe the same max table id).
//! Because `CatalogTxn` borrows the guard across its await, the whole
//! lock window runs on the blocking pool (`catalog_apply` below): the
//! executor's futures stay `Send`, which the MySQL shim requires.
//!
//! Physical rows of a dropped row-engine table are intentionally left
//! orphaned: the catalog tombstone makes them unreachable, and a
//! recreated table gets a fresh id, so orphans never alias a new table.
//! Columnar tables are different: their segment files would accumulate
//! forever, so DROP also purges the 0x23 metas, the registry entries
//! and the files (`columnar::commit::drop_table_segments`) once the
//! catalog drop has landed.

use std::sync::Arc;

use rocksdb::WriteBatch;

use crate::sql::exec::scan;
use crate::sql::exec::ExecOutcome;
use crate::sql::index::{self, IndexOps, IndexRef};
use crate::sql::parse::ast::{ColumnSpec, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog::{self, CatalogTxn};
use crate::sql::storage::row;
use crate::sql::storage::schema::{ColumnDef, Engine, IndexDef, SqlType, TableSchema};
use crate::state::Shared;
use crate::store::ops;

pub async fn run(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::CreateTable {
            name,
            if_not_exists,
            columns,
            pk,
            engine,
        } => create_table(shared, &name, if_not_exists, &columns, &pk, engine).await,
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

/// Run `begin` + the txn method while holding the raft write guard, on
/// the blocking pool (`CatalogTxn`'s guard borrow spans its await).
async fn catalog_apply(shared: &Shared, mutation: CatalogMutation) -> SqlResult<()> {
    let raft = Arc::clone(&shared.raft);
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut guard = raft.write().unwrap();
        let txn: CatalogTxn<'_> = catalog::begin(&mut guard, "DDL").map_err(SqlError::from)?;
        match mutation {
            CatalogMutation::Put(schema) => handle.block_on(txn.put(&schema)),
            CatalogMutation::Drop(schema) => handle.block_on(txn.drop(&schema.name, schema.id)),
            CatalogMutation::Kv { key, value } => handle.block_on(txn.put_kv(&key, &value)),
        }
        .map_err(SqlError::from)
    })
    .await
    .map_err(|e| SqlError::new(ErrorCode::Unknown, e.to_string()))?
}

async fn create_table(
    shared: &Shared,
    name: &str,
    if_not_exists: bool,
    columns: &[ColumnSpec],
    pk: &str,
    engine: Engine,
) -> SqlResult<ExecOutcome> {
    let schema = build_schema(0, name, columns, pk, engine)?;
    if catalog::lookup(shared, name)
        .map_err(SqlError::from)?
        .is_some()
    {
        if if_not_exists {
            return Ok(ExecOutcome::Ok);
        }
        return Err(SqlError::new(
            ErrorCode::TableExists,
            format!("table '{name}' already exists"),
        ));
    }
    let schema = TableSchema {
        id: alloc_table_id(shared),
        ..schema
    };
    catalog_apply(shared, CatalogMutation::Put(schema.clone())).await?;
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
    let schema = match catalog::lookup(shared, name).map_err(SqlError::from)? {
        Some(s) => s,
        None => {
            if if_exists {
                return Ok(ExecOutcome::Ok);
            }
            return Err(SqlError::no_such_table(name));
        }
    };
    catalog_apply(shared, CatalogMutation::Drop(schema.clone())).await?;
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
    let mut schema = lookup_table(shared, table)?;
    if schema.engine.is_columnar() {
        return Err(SqlError::new(
            ErrorCode::NotSupported,
            format!("indexes are not supported on columnar table '{table}'"),
        ));
    }
    if schema.index(name).is_some() {
        if if_not_exists {
            return Ok(ExecOutcome::Ok);
        }
        return Err(SqlError::new(
            ErrorCode::DupEntry,
            format!("index '{name}' already exists"),
        ));
    }
    if schema.column_index(column).is_none() {
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
    let index = IndexRef {
        name: name.to_string(),
        column: column.to_string(),
        unique,
    };
    if unique {
        let rows = scan::visible_rows(&shared.store, &schema, shared.sql_ts.now())?;
        index::maintain::assert_no_duplicates(&schema, &index, &rows)?;
    }
    let id = catalog::next_index_id(&schema);
    schema.indexes.push(IndexDef {
        id,
        name: name.to_string(),
        column: column.to_string(),
        unique,
    });
    catalog_apply(shared, CatalogMutation::Put(schema)).await?;
    // Backfill: rescan AFTER the catalog entry is committed, so every
    // row visible at this point is covered (any writer that started
    // earlier and lands later may miss its entry -- the accepted M2
    // race window; the residual WHERE filter hides stale entries, and
    // missing entries only cost the planner an index that finds fewer
    // pks than exist, which the fallback heuristic bounds).
    let schema = lookup_table(shared, table)?;
    backfill_index(shared, &schema, &index).await?;
    Ok(ExecOutcome::Ok)
}

/// Write index entries for every live row (leader-side, after the
/// catalog entry committed). One synced batch per whole backfill.
async fn backfill_index(shared: &Shared, schema: &TableSchema, index: &IndexRef) -> SqlResult<()> {
    let read_ts = shared.sql_ts.now();
    let rows = scan::visible_rows(&shared.store, schema, read_ts)?;
    let mut ops: IndexOps = Vec::with_capacity(rows.len());
    for r in &rows {
        let pk_key = row::pk_encode(&r[schema.pk_index()]).map_err(SqlError::from)?;
        ops.extend(index::entries_for_live_row(schema, index, &pk_key, r).map_err(SqlError::from)?);
    }
    if ops.is_empty() {
        return Ok(());
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
    let mut schema = lookup_table(shared, table)?;
    let Some(pos) = schema
        .indexes
        .iter()
        .position(|i| i.name.eq_ignore_ascii_case(name))
    else {
        if if_exists {
            return Ok(ExecOutcome::Ok);
        }
        return Err(SqlError::new(
            ErrorCode::Unknown,
            format!("index '{name}' doesn't exist"),
        ));
    };
    // Capture the column before the definition leaves the schema: the
    // on-disk keys are identified by (table_id, col_pos) alone.
    let col_pos = schema
        .column_index(&schema.indexes[pos].column)
        .ok_or_else(|| {
            SqlError::new(
                ErrorCode::BadField,
                format!("unknown column '{}'", schema.indexes[pos].column),
            )
        })?;
    // Removal by position keeps the remaining index ids stable. The
    // catalog entry goes first: if the entry sweep then fails, the
    // orphaned keys are unreachable (no index def) and harmless, while
    // the reverse order could leave a DECLARED index with no entries.
    let table_id = schema.id;
    schema.indexes.remove(pos);
    catalog_apply(shared, CatalogMutation::Put(schema)).await?;
    index::drop_entries(Arc::clone(&shared.store), table_id, col_pos as u32)
        .await
        .map_err(SqlError::from)?;
    Ok(ExecOutcome::Ok)
}

fn lookup_table(shared: &Shared, table: &str) -> SqlResult<TableSchema> {
    catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))
}

/// catalog::next_table_id takes `&Arc<Shared>`; the executor works with
/// a plain `&Shared`, so mirror its one-line max+1 here.
fn alloc_table_id(shared: &Shared) -> u32 {
    let live_max = catalog::list_tables(shared)
        .iter()
        .map(|s| s.id)
        .max()
        .unwrap_or(0);
    let dropped_max = catalog::dropped_ids(shared).into_iter().max().unwrap_or(0);
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
) -> SqlResult<TableSchema> {
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
        // if the body said NULL.
        defs.push(ColumnDef {
            name: c.name.clone(),
            sql_type: c.sql_type,
            nullable: c.nullable && i != pk_idx,
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::parse::error::ErrorCode;
    use crate::sql::parse::parse_statement;
    use crate::sql::storage::catalog;
    use crate::state::testutil;

    fn spec(name: &str, ty: crate::sql::storage::schema::SqlType, nullable: bool) -> ColumnSpec {
        ColumnSpec {
            name: name.to_string(),
            sql_type: ty,
            nullable,
            auto_increment: false,
        }
    }

    fn int_spec(name: &str) -> ColumnSpec {
        spec(name, crate::sql::storage::schema::SqlType::Int, true)
    }

    #[test]
    fn build_schema_validates_body() {
        use crate::sql::storage::schema::SqlType;
        let cols = [
            int_spec("id"),
            spec("v", SqlType::VarChar, true),
            spec("d", SqlType::Double, false),
        ];
        // missing pk column
        let err = build_schema(0, "t", &cols, "nope", Engine::Row).unwrap_err();
        assert_eq!(err.code, ErrorCode::Parse);
        assert!(err.msg.contains("primary key column 'nope'"));
        // duplicate column
        let dup = [int_spec("id"), int_spec("ID")];
        let err = build_schema(0, "t", &dup, "id", Engine::Row).unwrap_err();
        assert!(err.msg.contains("duplicate column"));
        // pk is implicitly NOT NULL even when declared NULL
        let s = build_schema(7, "t", &cols, "id", Engine::Row).unwrap();
        assert_eq!(s.id, 7);
        assert_eq!(s.pk, "id");
        assert!(!s.columns[0].nullable, "pk coerced NOT NULL");
        assert!(s.columns[1].nullable);
        assert!(!s.columns[2].nullable, "declared NOT NULL stays");
    }

    fn ai_spec(name: &str, ty: crate::sql::storage::schema::SqlType) -> ColumnSpec {
        ColumnSpec {
            auto_increment: true,
            ..spec(name, ty, true)
        }
    }

    /// MySQL 1075/1063 rules: at most one AUTO_INCREMENT column, integer
    /// type, and it must be the (single-column) primary key.
    #[test]
    fn build_schema_validates_auto_increment() {
        use crate::sql::storage::schema::SqlType;
        // legal: one integer AI column that is the pk
        let cols = [
            ai_spec("id", SqlType::Int),
            spec("v", SqlType::VarChar, true),
        ];
        let s = build_schema(1, "t", &cols, "id", Engine::Row).unwrap();
        assert_eq!(s.auto_increment.as_deref(), Some("id"));
        assert_eq!(s.auto_increment_index(), Some(0));

        // two AI columns -> 1075
        let dup = [
            ai_spec("id", SqlType::Int),
            ai_spec("seq", SqlType::Int),
            spec("v", SqlType::VarChar, true),
        ];
        let err = build_schema(0, "t", &dup, "id", Engine::Row).unwrap_err();
        assert_eq!(err.code, ErrorCode::WrongAutoKey);
        assert!(err.msg.contains("only one auto column"));

        // non-integer AI column -> incorrect column specifier
        let varchar = [
            ai_spec("id", SqlType::VarChar),
            spec("v", SqlType::Int, true),
        ];
        let err = build_schema(0, "t", &varchar, "id", Engine::Row).unwrap_err();
        assert_eq!(err.code, ErrorCode::WrongAutoKey);
        assert!(err.msg.contains("Incorrect column specifier"));

        // AI column not part of the (only supported) key -> 1075
        let not_pk = [
            spec("id", SqlType::Int, false),
            ai_spec("seq", SqlType::Int),
        ];
        let err = build_schema(0, "t", &not_pk, "id", Engine::Row).unwrap_err();
        assert_eq!(err.code, ErrorCode::WrongAutoKey);
        assert!(err.msg.contains("must be defined as a key"));
    }

    /// CREATE TABLE persists the schema flag AND the initial next-value
    /// counter (raft FSM entry `sql_sequence/<table>` = "1"); DROP clears
    /// it so a recreated table starts from 1 again.
    #[tokio::test]
    async fn create_auto_increment_table_persists_counter() {
        let shared = testutil::shared_with(testutil::test_config());
        run(
            &shared,
            parse_statement(
                "CREATE TABLE ai (id BIGINT AUTO_INCREMENT PRIMARY KEY, v VARCHAR(64) NULL)",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let s = catalog::lookup(&shared, "ai").unwrap().expect("created");
        assert_eq!(s.auto_increment.as_deref(), Some("id"));
        let raw = crate::state::raft_get(
            &shared.raft.read().unwrap(),
            &catalog::sequence_key(&s.name),
        );
        assert_eq!(raw, "1", "counter persisted alongside the schema");

        run(&shared, parse_statement("DROP TABLE ai").unwrap())
            .await
            .unwrap();
        let raw =
            crate::state::raft_get(&shared.raft.read().unwrap(), &catalog::sequence_key("ai"));
        assert_eq!(raw, "", "drop clears the counter (recreate starts at 1)");
    }

    #[tokio::test]
    async fn create_lookup_drop_round_trip() {
        let shared = testutil::shared_with(testutil::test_config());
        let stmt =
            parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)").unwrap();
        run(&shared, stmt).await.unwrap();
        let s = catalog::lookup(&shared, "t").unwrap().expect("created");
        assert_eq!(s.pk, "id");
        assert_eq!(s.id, 1, "first table id");

        // second table allocates a fresh id (max+1 over the stub kv)
        let stmt = parse_statement("CREATE TABLE u (id BIGINT PRIMARY KEY)").unwrap();
        run(&shared, stmt).await.unwrap();
        assert_eq!(catalog::lookup(&shared, "u").unwrap().unwrap().id, 2);

        // plain re-create fails; IF NOT EXISTS is a no-op
        let dup =
            parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)").unwrap();
        let err = run(&shared, dup).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::TableExists);
        let ine = parse_statement(
            "CREATE TABLE IF NOT EXISTS t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
        )
        .unwrap();
        assert!(matches!(run(&shared, ine).await.unwrap(), ExecOutcome::Ok));

        // DROP + tombstone: lookup misses, a repeat fails, IF EXISTS is fine
        let drop = parse_statement("DROP TABLE t").unwrap();
        assert!(matches!(run(&shared, drop).await.unwrap(), ExecOutcome::Ok));
        assert!(catalog::lookup(&shared, "t").unwrap().is_none());
        let again = parse_statement("DROP TABLE t").unwrap();
        let err = run(&shared, again).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NoSuchTable);
        let ine = parse_statement("DROP TABLE IF EXISTS t").unwrap();
        assert!(matches!(run(&shared, ine).await.unwrap(), ExecOutcome::Ok));
    }

    #[tokio::test]
    async fn drop_columnar_table_purges_segments() {
        let shared = testutil::shared_with(testutil::test_config());
        run(
            &shared,
            parse_statement(
                "CREATE TABLE cd (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL) ENGINE=columnar",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let schema = catalog::lookup(&shared, "cd").unwrap().unwrap();
        assert!(schema.engine.is_columnar());
        // Commit one segment the way the write path does.
        let rows = vec![vec![
            crate::sql::storage::schema::Value::Int(1),
            crate::sql::storage::schema::Value::Null,
        ]];
        let meta = crate::sql::columnar::writer::commit_segment(&shared, &schema, 7, 10, &rows)
            .await
            .unwrap();
        let dir = crate::sql::columnar::writer::columnar_dir(&shared.conf);
        assert!(dir.join(&meta.file).exists());
        assert_eq!(
            crate::sql::columnar::registry_of(&shared)
                .segments(schema.id)
                .len(),
            1
        );

        run(&shared, parse_statement("DROP TABLE cd").unwrap())
            .await
            .unwrap();
        assert!(catalog::lookup(&shared, "cd").unwrap().is_none());
        assert!(crate::sql::columnar::registry_of(&shared)
            .segments(schema.id)
            .is_empty());
        assert!(!dir.join(&meta.file).exists(), "segment file must be gone");
    }

    #[tokio::test]
    async fn create_and_drop_index_keeps_ids_stable() {
        let shared = testutil::shared_with(testutil::test_config());
        run(
            &shared,
            parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)").unwrap(),
        )
        .await
        .unwrap();
        run(
            &shared,
            parse_statement("CREATE INDEX i1 ON t (v)").unwrap(),
        )
        .await
        .unwrap();
        run(
            &shared,
            parse_statement("CREATE UNIQUE INDEX i2 ON t (v)").unwrap(),
        )
        .await
        .unwrap();
        let s = catalog::lookup(&shared, "t").unwrap().unwrap();
        let ids: Vec<u32> = s.indexes.iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(s.indexes[1].unique);

        // dropping i1 leaves i2's id untouched
        run(&shared, parse_statement("DROP INDEX i1 ON t").unwrap())
            .await
            .unwrap();
        let s = catalog::lookup(&shared, "t").unwrap().unwrap();
        assert_eq!(s.indexes.len(), 1);
        assert_eq!(s.indexes[0].id, 2);
        assert_eq!(s.indexes[0].name, "i2");

        // unknown index errors without IF EXISTS
        let err = run(&shared, parse_statement("DROP INDEX nope ON t").unwrap())
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unknown);
        assert!(matches!(
            run(
                &shared,
                parse_statement("DROP INDEX IF EXISTS nope ON t").unwrap()
            )
            .await
            .unwrap(),
            ExecOutcome::Ok
        ));
    }

    /// Monotone table ids (audit fix): DROP writes the dropped id as
    /// the tombstone value, so a re-created table -- same name or any
    /// other -- never reuses an issued id and can never alias the old
    /// table's orphaned row bytes. Runs the real CREATE/DROP path.
    #[tokio::test]
    async fn table_ids_stay_monotone_across_drop_recreate() {
        let shared = testutil::shared_with(testutil::test_config());
        let create_t = "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)";
        run(&shared, parse_statement(create_t).unwrap())
            .await
            .unwrap();
        assert_eq!(catalog::lookup(&shared, "t").unwrap().unwrap().id, 1);

        // same name, fresh id: the orphan-alias guard itself
        run(&shared, parse_statement("DROP TABLE t").unwrap())
            .await
            .unwrap();
        run(&shared, parse_statement(create_t).unwrap())
            .await
            .unwrap();
        assert_eq!(
            catalog::lookup(&shared, "t").unwrap().unwrap().id,
            2,
            "re-created table must not reuse the dropped id"
        );

        // ids keep counting past live AND tombstoned ids
        run(
            &shared,
            parse_statement("CREATE TABLE u (id BIGINT PRIMARY KEY)").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(catalog::lookup(&shared, "u").unwrap().unwrap().id, 3);
        run(&shared, parse_statement("DROP TABLE u").unwrap())
            .await
            .unwrap();
        run(
            &shared,
            parse_statement("CREATE TABLE v (id BIGINT PRIMARY KEY)").unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(catalog::lookup(&shared, "v").unwrap().unwrap().id, 4);

        // Tombstones live under the table-name key, so re-creating "t"
        // replaced id 1's tombstone with the live id-2 schema -- safe,
        // because 2 now bounds allocation. Only u's tombstone survives.
        assert_eq!(catalog::dropped_ids(&shared), vec![3]);
    }
}
