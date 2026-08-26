//! `build_schema` + catalog-persistence tests for StarRocks table
//! models (DUPLICATE KEY / PRIMARY KEY / DISTRIBUTED BY). Lives in a
//! sibling file to keep `ddl.rs` under the 800-line budget.

use super::super::ddl::{build_schema, run};
use crate::sql::parse::ast::ColumnSpec;
use crate::sql::parse::error::ErrorCode;
use crate::sql::parse::parse_statement;
use crate::sql::storage::catalog;
use crate::sql::storage::schema::{Distribution, Engine, KeyModel, SqlType};
use crate::state::testutil;

fn spec(name: &str, ty: SqlType, nullable: bool) -> ColumnSpec {
    ColumnSpec {
        name: name.to_string(),
        sql_type: ty,
        nullable,
        auto_increment: false,
    }
}

fn int_spec(name: &str) -> ColumnSpec {
    spec(name, SqlType::Int, true)
}

fn sr_model(
    kind: KeyModel,
    keys: &[&str],
    dist: Option<Distribution>,
) -> crate::sql::parse::starrocks::StarRocksModel {
    crate::sql::parse::starrocks::StarRocksModel {
        kind,
        keys: keys.iter().map(|k| k.to_string()).collect(),
        distribution: dist,
    }
}

fn dist(cols: &[&str], buckets: u32) -> Distribution {
    Distribution {
        columns: cols.iter().map(|c| c.to_string()).collect(),
        buckets,
    }
}

/// PK model: row engine, forced-NOT-NULL pk, distribution kept.
#[test]
fn build_schema_starrocks_pk_model() {
    let cols = [int_spec("k"), spec("v", SqlType::VarChar, true)];
    let m = sr_model(KeyModel::PrimaryKey, &["k"], Some(dist(&["k"], 8)));
    let s = build_schema(1, "t", &cols, "k", Engine::Row, Some(&m)).unwrap();
    assert_eq!(s.key_model, KeyModel::PrimaryKey);
    assert_eq!(s.engine, Engine::Row, "PK tables stay row-store");
    assert!(!s.columns[0].nullable, "PK coerced NOT NULL");
    assert_eq!(s.distribution.as_ref().unwrap().buckets, 8);
}

/// PK model cannot ride the columnar engine (upsert is impossible
/// there); dup model forces columnar regardless of ENGINE=olap.
#[test]
fn build_schema_model_engine_matrix() {
    let cols = [int_spec("k"), spec("v", SqlType::VarChar, true)];
    let pk = sr_model(KeyModel::PrimaryKey, &["k"], None);
    let e = build_schema(0, "t", &cols, "k", Engine::Columnar, Some(&pk)).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("PRIMARY KEY tables are row-store"));

    let dup = sr_model(KeyModel::Duplicate, &["k"], None);
    let s = build_schema(2, "t", &cols, "k", Engine::Row, Some(&dup)).unwrap();
    assert_eq!(s.engine, Engine::Columnar, "dup implies columnar");
    assert_eq!(s.key_model, KeyModel::Duplicate);
}

/// Dup model: the first dup-key column is the metadata pk and
/// KEEPS declared nullability (no dedup ever runs on it).
#[test]
fn build_schema_starrocks_dup_keeps_nullability() {
    let cols = [int_spec("k"), spec("v", SqlType::VarChar, true)];
    let m = sr_model(KeyModel::Duplicate, &["k", "v"], Some(dist(&["k"], 3)));
    let s = build_schema(3, "t", &cols, "k", Engine::Row, Some(&m)).unwrap();
    assert_eq!(s.pk, "k");
    assert!(
        s.columns[0].nullable,
        "dup-key pk is metadata, stays nullable"
    );
    assert_eq!(s.distribution.as_ref().unwrap().columns, ["k"]);
}

/// Distribution validation: unknown column, zero buckets.
#[test]
fn build_schema_validates_distribution() {
    let cols = [int_spec("k")];
    let bad_col = sr_model(KeyModel::Duplicate, &["k"], Some(dist(&["nope"], 2)));
    let e = build_schema(0, "t", &cols, "k", Engine::Row, Some(&bad_col)).unwrap_err();
    assert_eq!(e.code, ErrorCode::BadField);
    assert!(e.msg.contains("unknown column 'nope' in DISTRIBUTED BY"));

    let zero = sr_model(KeyModel::Duplicate, &["k"], Some(dist(&["k"], 0)));
    let e = build_schema(0, "t", &cols, "k", Engine::Row, Some(&zero)).unwrap_err();
    assert!(e.msg.contains("BUCKETS must be at least 1"));
}

/// Full parse->DDL round: StarRocks DDL lands in the catalog with
/// the model + distribution recorded.
#[tokio::test]
async fn starrocks_ddl_persists_model_in_catalog() {
    let shared = testutil::shared_with(testutil::test_config());
    run(
        &shared,
        parse_statement(
            "CREATE TABLE pk_t (k INT NOT NULL, v VARCHAR(64) NULL) \
             PRIMARY KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 8",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let s = catalog::lookup(&shared, "pk_t").unwrap().expect("created");
    assert_eq!(s.key_model, KeyModel::PrimaryKey);
    assert_eq!(s.pk, "k");
    assert_eq!(s.distribution.as_ref().unwrap().buckets, 8);

    run(
        &shared,
        parse_statement(
            "CREATE TABLE dup_t (k1 DATE, k2 INT, v VARCHAR(64)) \
             DUPLICATE KEY(k1, k2) DISTRIBUTED BY HASH(k2) BUCKETS 4",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let s = catalog::lookup(&shared, "dup_t").unwrap().expect("created");
    assert_eq!(s.key_model, KeyModel::Duplicate);
    assert_eq!(s.engine, Engine::Columnar);
    assert_eq!(s.pk, "k1");
    assert!(s.columns[0].nullable, "dup key keeps nullability");

    // multi-column PK model is Phase 4; the injected constraint
    // trips the single-pk check with its usual message (at parse
    // time -- translation is where the check lives).
    let e = parse_statement(
        "CREATE TABLE wide (a INT, b INT) PRIMARY KEY(a, b) DISTRIBUTED BY HASH(a)",
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::NotSupported);
    assert!(e.msg.contains("exactly one primary-key"), "{}", e.msg);
}
