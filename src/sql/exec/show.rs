//! SHOW TABLES / SHOW COLUMNS: thin catalog reads rendered as rowsets.

use crate::sql::exec::{ColMeta, ExecOutcome, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{SqlType, TableSchema, Value};
use crate::state::Shared;

/// Default schema name for the `Tables_in_<db>` column when no USE ran.
const DEFAULT_DB: &str = "rdb";

pub fn run(shared: &Shared, sess: &SqlSession, stmt: &Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::ShowTables => show_tables(shared, sess),
        Statement::ShowColumns(table) => show_columns(shared, table),
        Statement::ShowIndexes(table) => show_indexes(shared, table),
        _ => unreachable!("dispatch maps only SHOW statements here"),
    }
}

fn show_tables(shared: &Shared, sess: &SqlSession) -> SqlResult<ExecOutcome> {
    let db = if sess.db.is_empty() {
        DEFAULT_DB
    } else {
        sess.db.as_str()
    };
    Ok(ExecOutcome::Rows {
        columns: vec![ColMeta {
            table: String::new(),
            name: format!("Tables_in_{db}"),
            sql_type: SqlType::VarChar,
        }],
        rows: catalog::list_tables(shared)
            .into_iter()
            .map(|s| vec![Value::Str(s.name)])
            .collect(),
    })
}

fn show_columns(shared: &Shared, table: &str) -> SqlResult<ExecOutcome> {
    let schema = catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))?;
    let str_col = |name: &str| ColMeta {
        table: String::new(),
        name: name.to_string(),
        sql_type: SqlType::VarChar,
    };
    Ok(ExecOutcome::Rows {
        columns: vec![
            str_col("Field"),
            str_col("Type"),
            str_col("Null"),
            str_col("Key"),
            str_col("Default"),
            str_col("Extra"),
        ],
        rows: schema
            .columns
            .iter()
            .map(|c| {
                vec![
                    Value::Str(c.name.clone()),
                    Value::Str(type_name(c.sql_type).to_string()),
                    Value::Str(if c.nullable { "YES" } else { "NO" }.to_string()),
                    Value::Str(key_flag(&schema, &c.name).to_string()),
                    Value::Str("NULL".to_string()),
                    Value::Str(String::new()),
                ]
            })
            .collect(),
    })
}

/// MySQL `Key` flag: PRI for the primary key, UNI/MUL for indexed
/// columns (MUL marks a column whose index is non-unique or shared
/// with the pk -- single-column indexes make them the same thing).
fn key_flag(schema: &TableSchema, column: &str) -> &'static str {
    if column.eq_ignore_ascii_case(&schema.pk) {
        return "PRI";
    }
    match schema
        .indexes
        .iter()
        .find(|i| i.column.eq_ignore_ascii_case(column))
    {
        Some(i) if i.unique => "UNI",
        Some(_) => "MUL",
        None => "",
    }
}

/// SHOW INDEX FROM <table>: one row per index (M2: one column each),
/// MySQL-shaped columns.
fn show_indexes(shared: &Shared, table: &str) -> SqlResult<ExecOutcome> {
    let schema = catalog::lookup(shared, table)
        .map_err(SqlError::from)?
        .ok_or_else(|| SqlError::no_such_table(table))?;
    let str_col = |name: &str| ColMeta {
        table: String::new(),
        name: name.to_string(),
        sql_type: SqlType::VarChar,
    };
    let int_col = |name: &str| ColMeta {
        table: String::new(),
        name: name.to_string(),
        sql_type: SqlType::Int,
    };
    let mut rows = vec![vec![
        Value::Str(schema.name.clone()),
        Value::Int(0),
        Value::Str("PRIMARY".to_string()),
        Value::Int(1),
        Value::Str(schema.pk.clone()),
        Value::Str("BTREE".to_string()),
    ]];
    for i in &schema.indexes {
        rows.push(vec![
            Value::Str(schema.name.clone()),
            Value::Int(u8::from(i.unique) as i64),
            Value::Str(i.name.clone()),
            Value::Int(1),
            Value::Str(i.column.clone()),
            Value::Str("BTREE".to_string()),
        ]);
    }
    Ok(ExecOutcome::Rows {
        columns: vec![
            str_col("Table"),
            int_col("Non_unique"),
            str_col("Key_name"),
            int_col("Seq_in_index"),
            str_col("Column_name"),
            str_col("Index_type"),
        ],
        rows,
    })
}

/// MySQL-ish type names (narrow v1 domain).
fn type_name(t: SqlType) -> &'static str {
    match t {
        SqlType::Bool => "bool",
        SqlType::Int => "bigint",
        SqlType::Double => "double",
        SqlType::VarChar => "varchar",
        SqlType::Blob => "blob",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::exec::ddl;
    use crate::sql::parse::parse_statement;
    use crate::state::testutil;

    #[tokio::test]
    async fn show_tables_lists_created_tables_with_db_column() {
        let shared = testutil::shared_with(testutil::test_config());
        ddl::run(
            &shared,
            parse_statement("CREATE TABLE b (id BIGINT PRIMARY KEY)").unwrap(),
        )
        .await
        .unwrap();
        ddl::run(
            &shared,
            parse_statement("CREATE TABLE a (id BIGINT PRIMARY KEY)").unwrap(),
        )
        .await
        .unwrap();
        // dropped tables disappear (tombstone filtered)
        ddl::run(&shared, parse_statement("DROP TABLE b").unwrap())
            .await
            .unwrap();

        let Ok(ExecOutcome::Rows { columns, rows }) = run(
            &shared,
            &SqlSession {
                db: "mydb".into(),
                ..Default::default()
            },
            &parse_statement("SHOW TABLES").unwrap(),
        ) else {
            panic!("rows");
        };
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "Tables_in_mydb");
        assert_eq!(rows, vec![vec![Value::Str("a".into())]]);

        // no USE ran -> default db name
        let Ok(ExecOutcome::Rows { columns, .. }) = run(
            &shared,
            &SqlSession::default(),
            &parse_statement("SHOW TABLES").unwrap(),
        ) else {
            panic!("rows");
        };
        assert_eq!(columns[0].name, "Tables_in_rdb");
    }

    #[tokio::test]
    async fn show_columns_shape() {
        let shared = testutil::shared_with(testutil::test_config());
        ddl::run(
            &shared,
            parse_statement(
                "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, d DOUBLE NOT NULL)",
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let Ok(ExecOutcome::Rows { columns, rows }) = run(
            &shared,
            &SqlSession::default(),
            &parse_statement("SHOW COLUMNS FROM t").unwrap(),
        ) else {
            panic!("rows");
        };
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Field", "Type", "Null", "Key", "Default", "Extra"]
        );
        let cell = |r: usize, c: usize| match &rows[r][c] {
            Value::Str(s) => s.clone(),
            other => panic!("str expected, got {other:?}"),
        };
        assert_eq!(cell(0, 0), "id");
        assert_eq!(cell(0, 1), "bigint");
        assert_eq!(cell(0, 2), "NO", "pk is implicitly NOT NULL");
        assert_eq!(cell(0, 3), "PRI");
        assert_eq!(cell(1, 0), "v");
        assert_eq!(cell(1, 2), "YES");
        assert_eq!(cell(1, 3), "", "unindexed column carries no Key marker");
        assert_eq!(cell(2, 1), "double");
        assert_eq!(cell(2, 2), "NO", "declared NOT NULL");
        assert_eq!(cell(0, 4), "NULL");

        // unknown table errors
        let err = run(
            &shared,
            &SqlSession::default(),
            &parse_statement("SHOW COLUMNS FROM nope").unwrap(),
        )
        .unwrap_err();
        assert_eq!(err.code, crate::sql::parse::error::ErrorCode::NoSuchTable);
    }

    #[tokio::test]
    async fn show_indexes_lists_pk_and_secondary() {
        let shared = testutil::shared_with(testutil::test_config());
        ddl::run(
            &shared,
            parse_statement(
                "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, n BIGINT NULL)",
            )
            .unwrap(),
        )
        .await
        .unwrap();
        ddl::run(
            &shared,
            parse_statement("CREATE INDEX idx_v ON t (v)").unwrap(),
        )
        .await
        .unwrap();
        ddl::run(
            &shared,
            parse_statement("CREATE UNIQUE INDEX uq_n ON t (n)").unwrap(),
        )
        .await
        .unwrap();

        let stmt = parse_statement("SHOW INDEX FROM t").unwrap();
        let Ok(ExecOutcome::Rows { rows, .. }) = run(&shared, &SqlSession::default(), &stmt) else {
            panic!("rows");
        };
        let entry = |r: usize| match (&rows[r][1], &rows[r][2], &rows[r][4]) {
            (Value::Int(nu), Value::Str(k), Value::Str(c)) => (*nu, k.clone(), c.clone()),
            other => panic!("shape {other:?}"),
        };
        assert_eq!(entry(0), (0, "PRIMARY".to_string(), "id".to_string()));
        assert_eq!(entry(1), (0, "idx_v".to_string(), "v".to_string()));
        assert_eq!(entry(2), (1, "uq_n".to_string(), "n".to_string()));

        // SHOW COLUMNS Key flags follow the index kinds
        let Ok(ExecOutcome::Rows { rows, .. }) = run(
            &shared,
            &SqlSession::default(),
            &parse_statement("SHOW COLUMNS FROM t").unwrap(),
        ) else {
            panic!("rows");
        };
        let flag = |r: usize| match &rows[r][3] {
            Value::Str(s) => s.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(flag(0), "PRI");
        assert_eq!(flag(1), "MUL");
        assert_eq!(flag(2), "UNI");
    }
}
