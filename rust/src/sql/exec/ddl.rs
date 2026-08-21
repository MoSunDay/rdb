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
//! Physical rows of a dropped table are intentionally left orphaned:
//! the catalog tombstone makes them unreachable, and a recreated table
//! gets a fresh id, so orphans never alias a new table.

use std::sync::Arc;

use crate::sql::exec::ExecOutcome;
use crate::sql::parse::ast::{ColumnSpec, Statement};
use crate::sql::parse::error::{ErrorCode, SqlError, SqlResult};
use crate::sql::storage::catalog::{self, CatalogTxn};
use crate::sql::storage::schema::{ColumnDef, IndexDef, TableSchema};
use crate::state::Shared;

pub async fn run(shared: &Shared, stmt: Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::CreateTable {
            name,
            if_not_exists,
            columns,
            pk,
        } => create_table(shared, &name, if_not_exists, &columns, &pk).await,
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

/// One catalog mutation to apply under the DDL lock.
enum CatalogMutation {
    Put(TableSchema),
    Drop(String),
}

/// Run `begin` + the txn method while holding the raft write guard, on
/// the blocking pool (`CatalogTxn`'s guard borrow spans its await).
async fn catalog_apply(shared: &Shared, mutation: CatalogMutation) -> SqlResult<()> {
    let raft = Arc::clone(&shared.raft);
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let mut guard = raft.write().unwrap();
        let txn: CatalogTxn<'_> = catalog::begin(&mut guard).map_err(SqlError::from)?;
        match mutation {
            CatalogMutation::Put(schema) => handle.block_on(txn.put(&schema)),
            CatalogMutation::Drop(name) => handle.block_on(txn.drop(&name)),
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
) -> SqlResult<ExecOutcome> {
    let schema = build_schema(0, name, columns, pk)?;
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
    catalog_apply(shared, CatalogMutation::Put(schema)).await?;
    Ok(ExecOutcome::Ok)
}

async fn drop_table(shared: &Shared, name: &str, if_exists: bool) -> SqlResult<ExecOutcome> {
    if catalog::lookup(shared, name)
        .map_err(SqlError::from)?
        .is_none()
    {
        if if_exists {
            return Ok(ExecOutcome::Ok);
        }
        return Err(SqlError::no_such_table(name));
    }
    catalog_apply(shared, CatalogMutation::Drop(name.to_string())).await?;
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
    // M1 does NOT backfill existing rows into a new index (reads keep
    // using the pk scan until M2 adds backfill); the definition itself
    // is durable so M2's backfiller can find it. A second index on an
    // already-indexed column is allowed (no uniqueness of columns).
    let id = catalog::next_index_id(&schema);
    schema.indexes.push(IndexDef {
        id,
        name: name.to_string(),
        column: column.to_string(),
        unique,
    });
    catalog_apply(shared, CatalogMutation::Put(schema)).await?;
    Ok(ExecOutcome::Ok)
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
    // Removal by position keeps the remaining index ids stable.
    schema.indexes.remove(pos);
    catalog_apply(shared, CatalogMutation::Put(schema)).await?;
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
    catalog::list_tables(shared)
        .iter()
        .map(|s| s.id)
        .max()
        .unwrap_or(0)
        + 1
}

/// Validate a CREATE TABLE body and build its schema (id supplied by
/// the caller: 0 while validating, the allocated id before the put).
pub fn build_schema(
    id: u32,
    name: &str,
    columns: &[ColumnSpec],
    pk: &str,
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
        let err = build_schema(0, "t", &cols, "nope").unwrap_err();
        assert_eq!(err.code, ErrorCode::Parse);
        assert!(err.msg.contains("primary key column 'nope'"));
        // duplicate column
        let dup = [int_spec("id"), int_spec("ID")];
        let err = build_schema(0, "t", &dup, "id").unwrap_err();
        assert!(err.msg.contains("duplicate column"));
        // pk is implicitly NOT NULL even when declared NULL
        let s = build_schema(7, "t", &cols, "id").unwrap();
        assert_eq!(s.id, 7);
        assert_eq!(s.pk, "id");
        assert!(!s.columns[0].nullable, "pk coerced NOT NULL");
        assert!(s.columns[1].nullable);
        assert!(!s.columns[2].nullable, "declared NOT NULL stays");
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
}
