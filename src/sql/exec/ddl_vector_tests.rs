//! VECTOR(N) DDL matrix: accepted shapes (case-insensitive spelling,
//! dimension bounds, SHOW CREATE TABLE round trip) and the key-usage
//! rejections (PRIMARY KEY / AUTO_INCREMENT / secondary indexes /
//! columnar engine / foreign-key constraints).

use super::*;
use crate::sql::parse::ast::Statement;
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::parse_statement;
use crate::sql::storage::schema::SqlType;
use crate::state::testutil;

async fn err(sql: &str) -> crate::sql::parse::error::SqlError {
    let stmt = match parse_statement(sql) {
        Ok(s) => s,
        Err(e) => return e, // rejected at parse time is fine too
    };
    let shared = testutil::shared_with(testutil::test_config());
    run(&shared, stmt).await.expect_err("must reject")
}

#[tokio::test]
async fn create_table_with_vector_column_round_trip() {
    for (sql, dim) in [
        (
            "CREATE TABLE items (id BIGINT PRIMARY KEY, e VECTOR(3) NULL)",
            3u32,
        ),
        (
            "CREATE TABLE items (id BIGINT PRIMARY KEY, e vector(128) NOT NULL)",
            128,
        ),
        (
            "CREATE TABLE items (id BIGINT PRIMARY KEY, e Vector ( 4096 ) NULL)",
            4096,
        ),
    ] {
        let shared = testutil::shared_with(testutil::test_config());
        run(&shared, parse_statement(sql).expect("parse"))
            .await
            .expect("create");
        let s = catalog::lookup(&shared, "items").unwrap().expect("exists");
        assert_eq!(s.columns[1].sql_type, SqlType::Vector { dim }, "{sql}");

        // SHOW CREATE TABLE spells vector(N) and re-parses into the
        // same column type (the SHOW -> DDL round trip).
        let text = show::render_create_table(&s);
        assert!(text.contains(&format!("vector({dim})")), "{text}");
        let Statement::CreateTable { columns, pk, .. } = parse_statement(&text).unwrap() else {
            panic!("show output re-parses: {text}")
        };
        let rebuilt = build_schema(0, "items", &columns, &pk, Engine::Row, None).unwrap();
        assert_eq!(rebuilt.columns[1].sql_type, SqlType::Vector { dim });
    }
}

#[tokio::test]
async fn ddl_rejects_bad_vector_spellings() {
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR)",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3,4))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(0))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(4097))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(-2))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v widget(3))",
    ] {
        let e = err(sql).await;
        assert!(
            e.code == ErrorCode::NotSupported || e.code == ErrorCode::Parse,
            "{sql}: {e}"
        );
        assert!(
            e.msg.contains("VECTOR") || e.msg.contains("widget") || e.code == ErrorCode::Parse,
            "{sql}: {e}"
        );
    }
}

#[tokio::test]
async fn ddl_rejects_vector_key_usages() {
    // PRIMARY KEY, inline and table-constraint spellings.
    for sql in [
        "CREATE TABLE t (v VECTOR(3) PRIMARY KEY)",
        "CREATE TABLE t (v VECTOR(3), PRIMARY KEY (v))",
        "CREATE TABLE t (id BIGINT, v VECTOR(3), PRIMARY KEY (id, v))",
    ] {
        let e = err(sql).await;
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
        assert!(e.msg.contains("vector primary keys"), "{sql}: {e}");
    }
    // AUTO_INCREMENT.
    let e = err("CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3) AUTO_INCREMENT)").await;
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
    assert!(e.msg.contains("AUTO_INCREMENT on VECTOR"), "{e}");
    // Inline secondary indexes.
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3), KEY k (v))",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3), UNIQUE KEY k (v))",
    ] {
        let e = err(sql).await;
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
        assert!(e.msg.contains("VECTOR column"), "{sql}: {e}");
    }
    // CREATE INDEX on an existing vector column.
    let shared = testutil::shared_with(testutil::test_config());
    run(
        &shared,
        parse_statement("CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3) NULL)").unwrap(),
    )
    .await
    .unwrap();
    let e = run(&shared, parse_statement("CREATE INDEX k ON t (v)").unwrap())
        .await
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
    assert!(e.msg.contains("VECTOR column"), "{e}");
    // Foreign keys reject generically (vector column or not).
    let e = err("CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3), \
         FOREIGN KEY (v) REFERENCES u (x))")
    .await;
    assert_eq!(e.code, ErrorCode::NotSupported, "{e}");
    // Columnar engine has no vector encoding.
    let e = err("CREATE TABLE t (id BIGINT, v VECTOR(3)) ENGINE=columnar DUPLICATE KEY(id)").await;
    assert!(e.msg.contains("columnar"), "{e}");
}

#[test]
fn build_schema_rejects_vector_pk_and_columnar() {
    let spec = |name: &str, ty: SqlType| ColumnSpec {
        name: name.to_string(),
        sql_type: ty,
        nullable: true,
        auto_increment: false,
    };
    let int = spec("id", SqlType::Int);
    let vec = spec("v", SqlType::Vector { dim: 2 });
    // pk over the vector column (single and as part of a composite).
    let e = build_schema(
        0,
        "t",
        &[int.clone(), vec.clone()],
        &["v".into()],
        Engine::Row,
        None,
    )
    .unwrap_err();
    assert!(e.msg.contains("vector primary keys"), "{e}");
    let e = build_schema(
        0,
        "t",
        &[int.clone(), vec.clone()],
        &["id".into(), "v".into()],
        Engine::Row,
        None,
    )
    .unwrap_err();
    assert!(e.msg.contains("vector primary keys"), "{e}");
    // columnar engine.
    let e = build_schema(0, "t", &[int, vec], &["id".into()], Engine::Columnar, None).unwrap_err();
    assert!(e.msg.contains("columnar"), "{e}");
}
