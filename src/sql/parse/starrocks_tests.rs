//! Pre-parser unit tests: clause extraction, byte-identical
//! passthrough, rewrites, and loud rejections. AST-level behavior
//! (model attaching, DDL validation) lives in parse/mod.rs tests and
//! exec/ddl.rs tests.

use super::preparse;
use crate::sql::parse::error::ErrorCode;
use crate::sql::storage::schema::KeyModel;

fn model_of(sql: &str) -> crate::sql::parse::starrocks::StarRocksModel {
    preparse(sql).expect("preparse").1.expect("model")
}

#[test]
fn mysql_ddl_passes_through_byte_identical() {
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL)",
        "CREATE TABLE t (k VARCHAR(20), v INT, PRIMARY KEY (k)) ENGINE=columnar",
        "CREATE TABLE t (id BIGINT PRIMARY KEY) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        "SELECT * FROM t WHERE id = 1",
        "CREATE TABLE `weird``name` (`a b` INT, id INT PRIMARY KEY) COMMENT 'x,y(z)'",
    ] {
        let (text, model) = preparse(sql).expect("preparse");
        assert_eq!(text, sql, "must be byte-identical: {sql}");
        assert!(model.is_none(), "no StarRocks model expected: {sql}");
    }
}

#[test]
fn duplicate_key_extraction() {
    let sql = "CREATE TABLE detail (k1 DATE, k2 INT, v VARCHAR(64)) \
               DUPLICATE KEY(k1, k2) DISTRIBUTED BY HASH(k2) BUCKETS 8";
    let m = model_of(sql);
    assert_eq!(m.kind, KeyModel::Duplicate);
    assert_eq!(m.keys, ["k1", "k2"]);
    let d = m.distribution.expect("dist");
    assert_eq!(d.columns, ["k2"]);
    assert_eq!(d.buckets, 8);
    // The rewrite keeps the column list, drops both clauses.
    let (text, _) = preparse(sql).expect("preparse");
    assert_eq!(text, "CREATE TABLE detail (k1 DATE, k2 INT, v VARCHAR(64))");
}

#[test]
fn duplicate_key_keeps_tail_engine() {
    let sql = "CREATE TABLE d (k1 INT, v INT) DUPLICATE KEY(k1) ENGINE=olap \
               DISTRIBUTED BY HASH(k1) BUCKETS 3";
    let (text, m) = preparse(sql).expect("preparse");
    let m = m.expect("model");
    assert_eq!(m.keys, ["k1"]);
    assert_eq!(m.distribution.as_ref().expect("d").buckets, 3);
    assert_eq!(text, "CREATE TABLE d (k1 INT, v INT) ENGINE = olap");
}

#[test]
fn primary_key_injected_as_mysql_constraint() {
    let sql = "CREATE TABLE pk_t (k1 INT NOT NULL, v VARCHAR(10)) \
               PRIMARY KEY(k1) DISTRIBUTED BY HASH(k1) BUCKETS 10";
    let (text, m) = preparse(sql).expect("preparse");
    let m = m.expect("model");
    assert_eq!(m.kind, KeyModel::PrimaryKey);
    assert_eq!(m.keys, ["k1"]);
    assert_eq!(
        text,
        "CREATE TABLE pk_t (k1 INT NOT NULL, v VARCHAR(10), PRIMARY KEY (k1))"
    );
}

#[test]
fn case_insensitive_and_backticks() {
    let sql = "create table `T` (`K` int, v int) duplicate KEY(`K`) \
               Distributed By Hash(`K`) buckets 12";
    let m = model_of(sql);
    assert_eq!(m.kind, KeyModel::Duplicate);
    assert_eq!(m.keys, ["K"]);
    assert_eq!(m.distribution.expect("d").buckets, 12);
}

#[test]
fn buckets_defaults_to_starrocks_ten() {
    let m = model_of("CREATE TABLE t (k INT, v INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k)");
    assert_eq!(m.distribution.expect("d").buckets, 10);
}

#[test]
fn if_not_exists_is_preserved() {
    let sql = "CREATE TABLE IF NOT EXISTS t (k INT) DUPLICATE KEY(k) \
               DISTRIBUTED BY HASH(k) BUCKETS 1";
    let (text, m) = preparse(sql).expect("preparse");
    let m = m.expect("model");
    assert_eq!(m.kind, KeyModel::Duplicate);
    assert!(
        text.starts_with("CREATE TABLE IF NOT EXISTS t (k INT)"),
        "{text}"
    );
}

#[test]
fn multi_column_pk_is_extracted_verbatim() {
    // The rewrite injects PRIMARY KEY (a, b); the MySQL layer's
    // exactly-one-pk check rejects it downstream (tested in mod tests).
    let (text, m) = preparse(
        "CREATE TABLE t (a INT, b INT) PRIMARY KEY(a, b) DISTRIBUTED BY HASH(a) BUCKETS 2",
    )
    .expect("preparse");
    let m = m.expect("model");
    assert_eq!(m.keys, ["a", "b"]);
    assert!(text.contains("PRIMARY KEY (a, b)"), "{text}");
}

#[test]
fn unsupported_starrocks_clauses_reject_loudly() {
    for (sql, what) in [
        (
            "CREATE TABLE t (k INT) DUPLICATE KEY(k) PARTITION BY RANGE(k) ()",
            "PARTITION",
        ),
        (
            "CREATE TABLE t (k INT, v INT) DUPLICATE KEY(k) PROPERTIES (\"replication_num\" = \"1\")",
            "PROPERTIES",
        ),
        (
            "CREATE TABLE t (k INT) DUPLICATE KEY(k) ORDER BY (k) DISTRIBUTED BY HASH(k)",
            "ORDER BY",
        ),
        ("CREATE TABLE t (k INT) UNIQUE KEY(k) DISTRIBUTED BY HASH(k)", "UNIQUE KEY"),
        (
            "CREATE TABLE t (k INT) DUPLICATE KEY(k) DISTRIBUTED BY RANDOM BUCKETS 4",
            "RANDOM",
        ),
        ("CREATE TABLE t (k INT) DISTRIBUTED BY HASH(k) BUCKETS 4", "model"),
    ] {
        let e = preparse(sql).expect_err("must reject");
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
        assert!(e.msg.contains(what), "{sql}: {e}");
    }
}

#[test]
fn contradictory_shapes_reject() {
    for (sql, what) in [
        (
            "CREATE TABLE t (k INT) DUPLICATE KEY(k) PRIMARY KEY(k) DISTRIBUTED BY HASH(k)",
            "both",
        ),
        (
            "CREATE TABLE t (k INT PRIMARY KEY, v INT) DUPLICATE KEY(k) DISTRIBUTED BY HASH(k)",
            "cannot also declare",
        ),
        (
            "CREATE TABLE t (k INT) DUPLICATE KEY(k) ENGINE=row DISTRIBUTED BY HASH(k)",
            "columnar",
        ),
        (
            "CREATE TABLE t (k INT) DUPLICATE KEY(k) ENGINE=innodb",
            "columnar",
        ),
    ] {
        let e = preparse(sql).expect_err("must reject");
        assert_eq!(e.code, ErrorCode::NotSupported, "{sql}: {e}");
        assert!(e.msg.contains(what), "{sql}: {e}");
    }
}

#[test]
fn lexing_survives_tricky_literals() {
    // Comment inside the DDL, escaped quote in COMMENT, `--` inside a
    // string: none of these may confuse the clause scan.
    let sql = "CREATE TABLE t (k INT, /* c , ( ) */ v INT) DUPLICATE KEY(k) \
               COMMENT 'dup -- (key)' DISTRIBUTED BY HASH(k) BUCKETS 2";
    let (text, m) = preparse(sql).expect("preparse");
    let m = m.expect("model");
    assert_eq!(m.keys, ["k"]);
    assert!(text.ends_with("COMMENT 'dup -- (key)'"), "{text}");
}

#[test]
fn unterminated_constructs_are_parse_errors() {
    for sql in [
        "CREATE TABLE t (k INT) DUPLICATE KEY(k",
        "CREATE TABLE t (k INT) DUPLICATE KEY()",
        "CREATE TABLE t (k INT) DUPLICATE KEY(k,)",
    ] {
        assert!(preparse(sql).is_err(), "must reject: {sql}");
    }
}
