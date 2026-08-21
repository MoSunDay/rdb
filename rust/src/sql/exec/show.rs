//! SHOW TABLES / SHOW COLUMNS: thin catalog reads rendered as rowsets.

use crate::sql::exec::{ColMeta, ExecOutcome, SqlSession};
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::{SqlError, SqlResult};
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{SqlType, Value};
use crate::state::Shared;

/// Default schema name for the `Tables_in_<db>` column when no USE ran.
const DEFAULT_DB: &str = "rdb";

pub fn run(shared: &Shared, sess: &SqlSession, stmt: &Statement) -> SqlResult<ExecOutcome> {
    match stmt {
        Statement::ShowTables => show_tables(shared, sess),
        Statement::ShowColumns(table) => show_columns(shared, table),
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
                    // Secondary indexes surface here from M2 on.
                    Value::Str(
                        if c.name.eq_ignore_ascii_case(&schema.pk) {
                            "PRI"
                        } else {
                            ""
                        }
                        .to_string(),
                    ),
                    Value::Str("NULL".to_string()),
                    Value::Str(String::new()),
                ]
            })
            .collect(),
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
            &SqlSession { db: "mydb".into() },
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
            &SqlSession { db: String::new() },
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
            &SqlSession { db: String::new() },
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
        assert_eq!(cell(1, 3), "", "non-pk has no Key marker in M1");
        assert_eq!(cell(2, 1), "double");
        assert_eq!(cell(2, 2), "NO", "declared NOT NULL");
        assert_eq!(cell(0, 4), "NULL");

        // unknown table errors
        let err = run(
            &shared,
            &SqlSession { db: String::new() },
            &parse_statement("SHOW COLUMNS FROM nope").unwrap(),
        )
        .unwrap_err();
        assert_eq!(err.code, crate::sql::parse::error::ErrorCode::NoSuchTable);
    }
}
