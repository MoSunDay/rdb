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
    let err = build_schema(0, "t", &cols, "nope", Engine::Row, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::Parse);
    assert!(err.msg.contains("primary key column 'nope'"));
    // duplicate column
    let dup = [int_spec("id"), int_spec("ID")];
    let err = build_schema(0, "t", &dup, "id", Engine::Row, None).unwrap_err();
    assert!(err.msg.contains("duplicate column"));
    // pk is implicitly NOT NULL even when declared NULL
    let s = build_schema(7, "t", &cols, "id", Engine::Row, None).unwrap();
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
    let s = build_schema(1, "t", &cols, "id", Engine::Row, None).unwrap();
    assert_eq!(s.auto_increment.as_deref(), Some("id"));
    assert_eq!(s.auto_increment_index(), Some(0));

    // two AI columns -> 1075
    let dup = [
        ai_spec("id", SqlType::Int),
        ai_spec("seq", SqlType::Int),
        spec("v", SqlType::VarChar, true),
    ];
    let err = build_schema(0, "t", &dup, "id", Engine::Row, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongAutoKey);
    assert!(err.msg.contains("only one auto column"));

    // non-integer AI column -> incorrect column specifier
    let varchar = [
        ai_spec("id", SqlType::VarChar),
        spec("v", SqlType::Int, true),
    ];
    let err = build_schema(0, "t", &varchar, "id", Engine::Row, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::WrongAutoKey);
    assert!(err.msg.contains("Incorrect column specifier"));

    // AI column not part of the (only supported) key -> 1075
    let not_pk = [
        spec("id", SqlType::Int, false),
        ai_spec("seq", SqlType::Int),
    ];
    let err = build_schema(0, "t", &not_pk, "id", Engine::Row, None).unwrap_err();
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
    let raw = crate::state::raft_get(&shared.raft.read().unwrap(), &catalog::sequence_key("ai"));
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
    let ine =
        parse_statement("CREATE TABLE IF NOT EXISTS t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)")
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

/// DECIMAL columns land on the row engine (storage/encoding batch);
/// a DECIMAL pk and any DECIMAL column of a columnar table are loud
/// 1235 rejections, never mis-stored keys or pages.
#[test]
fn build_schema_guards_decimal_pk_and_columnar() {
    use crate::sql::storage::schema::SqlType;
    // DECIMAL pk: rejected even though the column itself is fine.
    let cols = [
        spec(
            "id",
            SqlType::Decimal {
                precision: 18,
                scale: 2,
            },
            false,
        ),
        int_spec("v"),
    ];
    let err = build_schema(0, "t", &cols, "id", Engine::Row, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotSupported);
    assert!(err.msg.contains("DECIMAL primary key"), "{err}");
    // Columnar engine + DECIMAL column: rejected.
    let cols = [
        int_spec("id"),
        spec(
            "amount",
            SqlType::Decimal {
                precision: 10,
                scale: 2,
            },
            true,
        ),
    ];
    let err = build_schema(0, "t", &cols, "id", Engine::Columnar, None).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotSupported);
    assert!(err.msg.contains("columnar"), "{err}");
    // Row engine + DECIMAL non-pk column: accepted.
    let s = build_schema(9, "t", &cols, "id", Engine::Row, None).unwrap();
    assert_eq!(s.columns[1].sql_type, cols[1].sql_type);
}

#[tokio::test]
async fn create_table_with_decimal_column_round_trip() {
    let shared = testutil::shared_with(testutil::test_config());
    run(
        &shared,
        parse_statement(
            "CREATE TABLE ledgers (id BIGINT PRIMARY KEY,\
             amount DECIMAL(18,4) NULL, rate NUMERIC(5,2) NOT NULL)",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let s = catalog::lookup(&shared, "ledgers")
        .unwrap()
        .expect("created");
    assert_eq!(s.pk, "id");
    assert_eq!(
        s.columns[1].sql_type,
        SqlType::Decimal {
            precision: 18,
            scale: 4
        }
    );
    assert!(s.columns[1].nullable);
    assert_eq!(
        s.columns[2].sql_type,
        SqlType::Decimal {
            precision: 5,
            scale: 2
        }
    );
    assert!(!s.columns[2].nullable);

    // The guards also fire end-to-end through the parser.
    let e = run(
        &shared,
        parse_statement("CREATE TABLE bad (d DECIMAL(10,2) PRIMARY KEY, v INT)").unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("DECIMAL primary key"), "{e}");
    let e = run(
        &shared,
        parse_statement(
            "CREATE TABLE bad2 (id BIGINT PRIMARY KEY, d DECIMAL(10,2)) ENGINE=columnar",
        )
        .unwrap(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("columnar"), "{e}");
}
