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
            "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, d DOUBLE NOT NULL, \
             amount DECIMAL(18,4) NULL)",
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
    // DECIMAL spells its full width/scale form.
    assert_eq!(cell(3, 0), "amount");
    assert_eq!(cell(3, 1), "decimal(18,4)");
    assert_eq!(cell(3, 2), "YES");

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
    assert_eq!(entry(1), (1, "idx_v".to_string(), "v".to_string()));
    assert_eq!(entry(2), (0, "uq_n".to_string(), "n".to_string()));

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

async fn show_rows(shared: &crate::state::Shared, sess: &SqlSession, sql: &str) -> Vec<Vec<Value>> {
    match run(shared, sess, &parse_statement(sql).unwrap()).unwrap() {
        ExecOutcome::Rows { rows, .. } => rows,
        other => panic!("rows expected, got {other:?}"),
    }
}

#[tokio::test]
async fn show_create_table_renders_deterministic_round_trip_ddl() {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement(
            "CREATE TABLE t (id BIGINT AUTO_INCREMENT PRIMARY KEY, \
             v VARCHAR(64) NULL, n BIGINT NULL, d DATE NOT NULL)",
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

    let rows = show_rows(&shared, &SqlSession::default(), "SHOW CREATE TABLE t").await;
    assert_eq!(rows.len(), 1);
    let Value::Str(text) = &rows[0][1] else {
        panic!("create text");
    };
    let expected = "CREATE TABLE `t` (\n\
                    \x20 `id` bigint NOT NULL AUTO_INCREMENT,\n\
                    \x20 `v` varchar NULL DEFAULT NULL,\n\
                    \x20 `n` bigint NULL DEFAULT NULL,\n\
                    \x20 `d` date NOT NULL,\n\
                    \x20 PRIMARY KEY (`id`),\n\
                    \x20 KEY `idx_v` (`v`),\n\
                    \x20 UNIQUE KEY `uq_n` (`n`)\n\
                    ) ENGINE=InnoDB";
    assert_eq!(text, expected);
    // determinism: same schema renders the same text
    let again = show_rows(&shared, &SqlSession::default(), "SHOW CREATE TABLE t").await;
    assert_eq!(again[0][1], rows[0][1]);
    // ROUND TRIP: the rendered DDL re-creates an equivalent table
    let mut made = text.replace("CREATE TABLE `t`", "CREATE TABLE `rt`");
    made = made.replace("`t`", "`rt`");
    ddl::run(&shared, parse_statement(&made).unwrap())
        .await
        .unwrap();
    let orig = catalog::lookup(&shared, "t").unwrap().unwrap();
    let rt = catalog::lookup(&shared, "rt").unwrap().unwrap();
    assert_eq!(rt.columns, orig.columns);
    assert_eq!(rt.pk, orig.pk);
    assert_eq!(rt.auto_increment, orig.auto_increment);
    assert_eq!(rt.indexes, orig.indexes);
    assert_eq!(rt.engine, orig.engine);

    // unknown table errors
    let err = run(
        &shared,
        &SqlSession::default(),
        &parse_statement("SHOW CREATE TABLE nope").unwrap(),
    )
    .unwrap_err();
    assert_eq!(err.code, crate::sql::parse::error::ErrorCode::NoSuchTable);
}

#[tokio::test]
async fn show_create_table_renders_starrocks_and_columnar_models() {
    let shared = testutil::shared_with(testutil::test_config());
    ddl::run(
        &shared,
        parse_statement(
            "CREATE TABLE dup (k BIGINT, v BIGINT) DUPLICATE KEY(k) \
             DISTRIBUTED BY HASH(k) BUCKETS 8 ENGINE=columnar",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let rows = show_rows(&shared, &SqlSession::default(), "SHOW CREATE TABLE dup").await;
    let Value::Str(text) = &rows[0][1] else {
        panic!("create text");
    };
    assert!(
        text.contains("DUPLICATE KEY(`k`) DISTRIBUTED BY HASH(`k`) BUCKETS 8 ENGINE=columnar"),
        "{text}"
    );
    // round trip through the pre-parser
    let made = text.replace("CREATE TABLE `dup`", "CREATE TABLE `dup2`");
    ddl::run(&shared, parse_statement(&made).unwrap())
        .await
        .unwrap();
    let a = catalog::lookup(&shared, "dup").unwrap().unwrap();
    let b = catalog::lookup(&shared, "dup2").unwrap().unwrap();
    assert_eq!(b.key_model, a.key_model);
    assert_eq!(b.distribution, a.distribution);
    assert_eq!(b.engine, a.engine);
}

#[tokio::test]
async fn show_databases_lists_default_and_session_db() {
    let shared = testutil::shared_with(testutil::test_config());
    let of = |rows: Vec<Vec<Value>>| -> Vec<String> {
        rows.into_iter()
            .map(|mut r| match r.remove(0) {
                Value::Str(s) => s,
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let default = show_rows(&shared, &SqlSession::default(), "SHOW DATABASES").await;
    assert_eq!(of(default), vec!["rdb"]);
    let used = SqlSession {
        db: "mydb".into(),
        ..Default::default()
    };
    let picked = show_rows(&shared, &used, "SHOW DATABASES").await;
    assert_eq!(of(picked), vec!["rdb", "mydb"]);
}

#[tokio::test]
async fn show_variables_and_status_with_like_filter() {
    let shared = testutil::shared_with(testutil::test_config());
    let sess = SqlSession::default();
    let all = show_rows(&shared, &sess, "SHOW VARIABLES").await;
    assert!(all.len() >= 20, "the sysvar table is the row source");
    assert!(all.iter().all(|r| matches!(&r[1], Value::Str(_))));

    let wait = show_rows(&shared, &sess, "SHOW VARIABLES LIKE 'wait%'").await;
    assert_eq!(wait.len(), 1);
    assert_eq!(wait[0][0], Value::Str("wait_timeout".into()));
    assert_eq!(wait[0][1], Value::Str("28800".into()));

    // string-valued variables pass through verbatim (no Debug quotes)
    let sql_mode = show_rows(&shared, &sess, "SHOW VARIABLES LIKE 'sql_mode'").await;
    assert_eq!(sql_mode[0][1], Value::Str(String::new()));
    let tz = show_rows(&shared, &sess, "SHOW VARIABLES LIKE 'time_zone'").await;
    assert_eq!(tz[0][1], Value::Str("SYSTEM".into()));

    // `_` matches one char; matching is case-insensitive (SHOW-only)
    let one = show_rows(&shared, &sess, "SHOW VARIABLES LIKE 'VERS_ON'").await;
    assert_eq!(one.len(), 1, "underscore wildcard + case folding");
    let upper = show_rows(&shared, &sess, "SHOW VARIABLES LIKE 'WAIT%'").await;
    assert_eq!(upper.len(), 1, "pattern case folding");

    let status = show_rows(&shared, &sess, "SHOW STATUS").await;
    assert_eq!(status[0][0], Value::Str("Uptime".into()));
    let filtered = show_rows(&shared, &sess, "SHOW GLOBAL STATUS LIKE 'Upt%'").await;
    assert_eq!(filtered.len(), 1);
}
