//! VECTOR(N) end-to-end over one real rdb process (P2c): DDL surface
//! and metadata, INSERT coercion, literal/function round-trips over
//! the wire, exact KNN ORDER BY DISTANCE with LIMIT, and the error
//! matrix (dim mismatch, bad literal, wrong arg counts, VECTOR not a
//! key). The MySQL face is text-only: a vector travels as '[x,y,z]'.

use crate::common;

use common::mysql::{
    ddl, rows, run, s, ER_TRUNCATED_WRONG_VALUE, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT, PASS,
};
use common::{
    mysql_root_conn, mysql_server_error, spawn_node_mysql, wait_mysql_ready, wait_resp_ready,
};

#[tokio::test]
async fn vector_surface_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-vector-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // DDL: dims 1..=4096; metadata carries VECTOR(N); rendering is
    // '[a,b,c]' with f32 shortest-round-trip text.
    ddl(
        &mut c,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, v VECTOR(3) NULL)",
    )
    .await;
    assert_eq!(
        rows(&mut c, "DESCRIBE items").await[1][..2],
        vec![s("v"), s("vector(3)")]
    );
    ddl(
        &mut c,
        "CREATE TABLE wide (id BIGINT PRIMARY KEY, v VECTOR(4096) NULL)",
    )
    .await;
    ddl(&mut c, "DROP TABLE wide").await;
    ddl(&mut c, "DROP TABLE items").await;

    ddl(
        &mut c,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, v VECTOR(3) NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO items VALUES (1, '[1,2,3]'), (2, '[0,0,0]'), (3, '[-1,2,0.5]')",
    )
    .await;
    assert_eq!(
        rows(&mut c, "SELECT id, v FROM items ORDER BY id").await,
        vec![
            vec![s("1"), s("[1,2,3]")],
            vec![s("2"), s("[0,0,0]")],
            vec![s("3"), s("[-1,2,0.5]")],
        ]
    );
    // TO_VECTOR is the STRING_TO_VECTOR alias; VECTOR_TO_STRING is the
    // VEC_TO_TEXT alias (both names must behave identically).
    assert_eq!(
        rows(&mut c, "SELECT STRING_TO_VECTOR('[4,5,6]') = TO_VECTOR('[4,5,6]'), VECTOR_TO_STRING(v) FROM items WHERE id = 1").await,
        vec![vec![s("1"), s("[1,2,3]")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT VECTOR_DIM(v) FROM items WHERE id = 3").await,
        vec![vec![s("3")]]
    );

    // Bad literals: 1292 on malformed or wrong-dimension vectors.
    let e = mysql_server_error(&mut c, "INSERT INTO items VALUES (9, '[1,2]')").await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE);
    let e = mysql_server_error(&mut c, "SELECT TO_VECTOR('nope')").await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE);

    // Functions: arg counts (1582) and dimension mismatch between
    // operands.
    assert_eq!(
        rows(&mut c, "SELECT VECTOR_DIM('[1,2]')").await,
        vec![vec![s("2")]]
    );
    let e = mysql_server_error(&mut c, "SELECT VECTOR_DIM(v, v) FROM items WHERE id = 1").await;
    assert_eq!(e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT);
    let e = mysql_server_error(
        &mut c,
        "SELECT DISTANCE(v, '[1,2]', 'cosine') FROM items WHERE id = 1",
    )
    .await;
    assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE);

    // Exact KNN, cosine against [1,0,0]: sims are id1=1/sqrt(14)=0.267
    // (distance 0.73), id2=zero vector (distance 1 by definition),
    // id3=-1/sqrt(5.25)=-0.436 (distance 1.44) -- ascending order 1,2,3.
    assert_eq!(
        rows(
            &mut c,
            "SELECT id FROM items ORDER BY DISTANCE(v, '[1,0,0]', 'cosine') LIMIT 2"
        )
        .await,
        vec![vec![s("1")], vec![s("2")]]
    );
    // Euclidean: id2 is |[1,0,0]|=1 away, id3 sqrt(8.25)=2.87, id1
    // sqrt(13)=3.61 -- the zero vector is the nearest neighbor.
    assert_eq!(
        rows(
            &mut c,
            "SELECT id FROM items ORDER BY DISTANCE(v, '[1,0,0]', 'euclidean') LIMIT 1"
        )
        .await,
        vec![vec![s("2")]]
    );
    // 'dot' is negated inner product (bigger inner product first).
    assert_eq!(
        rows(
            &mut c,
            "SELECT id FROM items ORDER BY DISTANCE(v, '[1,0,0]', 'dot') LIMIT 1"
        )
        .await,
        vec![vec![s("1")]]
    );
    // Metric is required: 2-arg DISTANCE is not the MySQL face.
    let e = mysql_server_error(
        &mut c,
        "SELECT DISTANCE(v, '[1,0,0]') FROM items WHERE id = 1",
    )
    .await;
    assert_eq!(e.code, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT);

    // VECTOR is an equality-only domain: never a key.
    let e = mysql_server_error(&mut c, "CREATE TABLE bad (v VECTOR(3) PRIMARY KEY)").await;
    assert_ne!(e.code, 0);

    ddl(&mut c, "DROP TABLE items").await;
}

/// P4 ANN over the wire: CREATE VECTOR INDEX backfills from live rows,
/// DML after the index stays visible to indexed queries, the probe's
/// top-k equals the exact scan's top-k on clustered data, and DROP
/// INDEX returns the table to the exact path without residue errors.
#[tokio::test]
async fn vector_ann_index_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-vector-ann-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    ddl(
        &mut c,
        "CREATE TABLE vec (id BIGINT PRIMARY KEY, v VECTOR(2) NOT NULL)",
    )
    .await;
    // 3 tight clusters of 4 rows each, well separated.
    let mut sql = String::from("INSERT INTO vec VALUES ");
    for (cid, base) in [(1, 0.0f64), (2, 50.0), (3, 100.0)] {
        for j in 0..4 {
            if cid > 1 || j > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!(
                "({}, '[{:.2},{:.2}]')",
                cid * 10 + j,
                base + j as f64 * 0.5,
                base + 3.0 + j as f64 * 0.5
            ));
        }
    }
    run(&mut c, &sql).await;
    ddl(
        &mut c,
        "CREATE VECTOR INDEX vidx ON vec (v) WITH (METRIC 'euclidean', NPROBE 1)",
    )
    .await;
    // duplicate name / unknown column / non-VECTOR column all reject.
    let e = mysql_server_error(&mut c, "CREATE VECTOR INDEX vidx ON vec (v)").await;
    assert_eq!(e.code, common::mysql::ER_DUP_ENTRY);
    let e = mysql_server_error(&mut c, "CREATE VECTOR INDEX vidx2 ON vec (nope)").await;
    assert_eq!(e.code, common::mysql::ER_BAD_FIELD_ERROR);
    let e = mysql_server_error(&mut c, "CREATE VECTOR INDEX vidx2 ON vec (id)").await;
    assert_eq!(e.code, common::mysql::ER_NOT_SUPPORTED_YET);

    // DML through the index stays visible to indexed queries.
    run(&mut c, "INSERT INTO vec VALUES (40, '[0.10,3.20]')").await;
    run(&mut c, "DELETE FROM vec WHERE id = 11").await;
    ddl(
        &mut c,
        "CREATE TABLE scratch (id BIGINT PRIMARY KEY, v VECTOR(2) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO scratch VALUES (1, '[999,999]')").await;
    run(&mut c, "DROP TABLE scratch").await;

    // ANN top-3 == exact top-3 for a query sitting on cluster 1 (rows
    // 10,12,13 after deleting 11).
    let got = rows(
        &mut c,
        "SELECT id FROM vec ORDER BY DISTANCE(v, '[0,3]', 'euclidean') LIMIT 3",
    )
    .await;
    assert_eq!(got, vec![vec![s("10")], vec![s("40")], vec![s("12")]]);

    // metric mismatch (index is euclidean, query cosine) silently uses
    // the exact path with the same winner set.
    let got = rows(
        &mut c,
        "SELECT id FROM vec ORDER BY DISTANCE(v, '[0,3]', 'cosine') LIMIT 3",
    )
    .await;
    assert_eq!(got, vec![vec![s("10")], vec![s("40")], vec![s("12")]]);

    // DROP INDEX sweeps and the exact path still answers.
    ddl(&mut c, "DROP INDEX vidx ON vec").await;
    let got = rows(
        &mut c,
        "SELECT id FROM vec ORDER BY DISTANCE(v, '[0,3]', 'euclidean') LIMIT 3",
    )
    .await;
    assert_eq!(got, vec![vec![s("10")], vec![s("40")], vec![s("12")]]);
}
