//! DECIMAL exact-semantics e2e over one real rdb process: int.frac
//! literals never touch an f64, column writes round half away from
//! zero at the declared scale, metadata/DESCRIBE carry the type,
//! cross-scale comparisons drive a secondary index, numeric ORDER BY,
//! SUM/AVG scale rules, NULL propagation, and precision enforcement
//! (MySQL 1292). Split out of `sql_types_e2e.rs` (M5 file-size cap);
//! behavior is byte-identical to the test that lived there.

mod common;

use common::mysql::{col_types, ddl, rows, s, PASS};
use common::{
    mysql_root_conn, mysql_server_error, spawn_node_mysql, wait_mysql_ready, wait_resp_ready,
};
use mysql_async::consts::ColumnType::*;
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

#[tokio::test]
async fn decimal_exact_semantics_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-types-dec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // Exact literals: plain int.frac text never touches an f64. The sum
    // renders at scale max(1,1)=1 like MySQL, and 0.1+0.2 is exactly 0.3.
    assert_eq!(rows(&mut c, "SELECT 0.1 + 0.2").await, vec![vec![s("0.3")]]);
    assert_eq!(rows(&mut c, "SELECT 1 / 3").await, vec![vec![s("0")]]);
    assert_eq!(
        rows(&mut c, "SELECT 1.0 / 3").await,
        vec![vec![s("0.33333")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT -0.05 / 0.10").await,
        vec![vec![s("-0.500000")]]
    );

    // Column writes round half away from zero at the declared scale.
    ddl(
        &mut c,
        "CREATE TABLE dec (id BIGINT PRIMARY KEY, v DECIMAL(10,2))",
    )
    .await;
    for (id, v) in [(1, "1.005"), (2, "2.344"), (3, "-1.005"), (4, "NULL")] {
        c.query_drop(format!("INSERT INTO dec (id, v) VALUES ({id}, {v})"))
            .await
            .expect("seed dec");
    }
    assert_eq!(
        rows(&mut c, "SELECT v FROM dec ORDER BY id").await,
        vec![
            vec![s("1.01")],
            vec![s("2.34")],
            vec![s("-1.01")],
            vec![MVal::NULL]
        ]
    );

    // Metadata and DESCRIBE carry the declared type.
    assert_eq!(
        col_types(&mut c, "SELECT v FROM dec").await,
        vec![MYSQL_TYPE_NEWDECIMAL]
    );
    assert_eq!(
        rows(&mut c, "DESCRIBE dec").await[0][..2],
        vec![s("id"), s("bigint")][..]
    );
    assert_eq!(
        rows(&mut c, "DESCRIBE dec").await[1][..2],
        vec![s("v"), s("decimal(10,2)")][..]
    );

    // Exact comparisons drive a secondary index (cross-scale literal).
    ddl(&mut c, "CREATE INDEX idx_v ON dec (v)").await;
    assert_eq!(
        rows(&mut c, "SELECT id FROM dec WHERE v = 1.010").await,
        vec![vec![s("1")]]
    );
    let rs = rows(&mut c, "EXPLAIN SELECT id FROM dec WHERE v = 1.010").await;
    let MVal::Bytes(b) = &rs[0][0] else {
        panic!("plan line")
    };
    assert_eq!(
        String::from_utf8(b.clone()).unwrap(),
        "IndexScan idx_v -> 1 pks"
    );

    // ORDER BY walks the numeric order, negatives first.
    assert_eq!(
        rows(&mut c, "SELECT v FROM dec WHERE v IS NOT NULL ORDER BY v").await,
        vec![vec![s("-1.01")], vec![s("1.01")], vec![s("2.34")]]
    );

    // SUM stays decimal at the column scale; AVG divides at scale+4.
    assert_eq!(
        rows(&mut c, "SELECT SUM(v) FROM dec").await,
        vec![vec![s("2.34")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT AVG(v) FROM dec").await,
        vec![vec![s("0.780000")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT SUM(v) FROM dec WHERE id = 4").await,
        vec![vec![MVal::NULL]]
    );
    assert_eq!(
        rows(&mut c, "SELECT v + 1 FROM dec WHERE id = 4").await,
        vec![vec![MVal::NULL]]
    );

    // The declared precision is enforced after rounding (MySQL 1292).
    ddl(
        &mut c,
        "CREATE TABLE p5 (id BIGINT PRIMARY KEY, v DECIMAL(5,2))",
    )
    .await;
    c.query_drop("INSERT INTO p5 (id, v) VALUES (1, 999.99)")
        .await
        .expect("edge fits");
    for sql in [
        "INSERT INTO p5 (id, v) VALUES (2, 123456)",
        "INSERT INTO p5 (id, v) VALUES (2, 1000)",
        "UPDATE p5 SET v = 999.995 WHERE id = 1",
    ] {
        let e = mysql_server_error(&mut c, sql).await;
        assert_eq!(e.code, 1292, "{sql}: {}", e.message);
        assert!(e.message.contains("Out of range"), "{sql}: {}", e.message);
    }
    assert_eq!(
        rows(&mut c, "SELECT COUNT(*) FROM p5").await,
        vec![vec![s("1")]]
    );
    node.kill_now();
}
