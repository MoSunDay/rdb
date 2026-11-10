//! T4 turso-engine vector surface over one real rdb process (see
//! plans/2026-11-09-turso-engine): VECTOR(N) DDL + DESCRIBE, the
//! blob-backed write path with app-side dimension validation, the
//! STRING_TO_VECTOR/VECTOR_TO_STRING/VECTOR_DIM/DISTANCE function
//! family on turso's own vector functions, exact KNN (ORDER BY
//! DISTANCE ... LIMIT), the 1292 error matrix, CREATE VECTOR INDEX's
//! 1235, and id-ordering parity against the rocks engine fed the
//! same table and queries.
//!
//! Float discipline: turso computes distances in f32 (simsimd) where
//! rocks accumulates in f64, and cosine of IDENTICAL vectors reads
//! ~1.79e-7, never 0 -- every numeric assertion is epsilon-based
//! (1e-6), never exact equality.

mod common;

use common::mysql::{
    ddl, rows, run, s, spawn_node_mysql_yaml, ER_TRUNCATED_WRONG_VALUE, ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT,
    PASS,
};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::Queryable;

/// First cell of the first row as f64 (distances ship as text).
async fn scalar_f64(c: &mut mysql_async::Conn, sql: &str) -> f64 {
    let r = rows(c, sql).await;
    let cell = r
        .first()
        .and_then(|row| row.first())
        .expect("non-empty result");
    match cell {
        mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("{sql}: not a text scalar: {other:?}"),
    }
}

/// |a - b| within the suite's epsilon.
fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// The ids of one-column KNN results, in arrival order.
fn ids(rs: &Vec<Vec<mysql_async::Value>>) -> Vec<String> {
    rs.iter()
        .map(|r| match &r[0] {
            mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// 8 vectors in R^3 with well-separated cosine AND euclidean distance
/// orderings against the probe [1,0,0] (no ties, margins >> float
/// noise; ids 1-3 are the rocks sql_vector_e2e dataset).
const DATASET: &[(&str, &str)] = &[
    ("1", "[1,2,3]"),
    ("2", "[0,0,0]"),
    ("3", "[-1,2,0.5]"),
    ("4", "[-5,-5,-5]"),
    ("5", "[0.2,-3,0]"),
    ("6", "[-2,0,-3]"),
    ("7", "[-10,10,5]"),
    ("8", "[-1,-1,0.25]"),
];

/// Create the shared table and load DATASET on one connection.
async fn setup(c: &mut mysql_async::Conn) {
    ddl(c, "CREATE TABLE vecs (id INT PRIMARY KEY, v VECTOR(3) NOT NULL)").await;
    let values = DATASET
        .iter()
        .map(|(id, v)| format!("({id}, '{v}')"))
        .collect::<Vec<_>>()
        .join(", ");
    run(c, &format!("INSERT INTO vecs VALUES {values}")).await;
}

#[tokio::test]
async fn turso_vector_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-vector-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let turso_dir = dir.join("tursodb");
    std::fs::create_dir_all(&turso_dir).unwrap();
    let extra = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let mut node = spawn_node_mysql_yaml(&dir, 0, true, None, &extra);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;
    // The parity reference: same node, default (rocks) database.
    let mut r = mysql_root_conn(&node, PASS).await;

    common::mysql::ddl(&mut c, "CREATE DATABASE turso_vec").await;
    c.query_drop("USE turso_vec").await.expect("use turso_vec");

    // (a) DDL + DESCRIBE: the dim is recovered from the CREATE TABLE
    // text (table_info normalizes down to bare `vector`), so both
    // engines spell the MySQL face `vector(3)`.
    setup(&mut c).await;
    let desc = rows(&mut c, "DESCRIBE vecs").await;
    assert_eq!(desc[1][..2], vec![s("v"), s("vector(3)")], "{desc:?}");

    // (b) Read-back: stored blobs decode to the canonical '[e,e,...]'
    // text, byte-identical to the rocks engine's spellings.
    let got = rows(&mut c, "SELECT id, v FROM vecs ORDER BY id").await;
    let want: Vec<Vec<mysql_async::Value>> = DATASET
        .iter()
        .map(|(id, v)| vec![s(id), s(v)])
        .collect();
    assert_eq!(got, want);
    setup(&mut r).await;
    assert_eq!(
        rows(&mut r, "SELECT id, v FROM vecs ORDER BY id").await,
        want,
        "read-back parity with rocks"
    );

    // (c) Function family.
    assert_eq!(
        rows(&mut c, "SELECT STRING_TO_VECTOR('[4,5,6]') = TO_VECTOR('[4,5,6]'), VECTOR_TO_STRING(v), VECTOR_DIM(v) FROM vecs WHERE id = 1").await,
        vec![vec![s("1"), s("[1,2,3]"), s("3")]]
    );
    // turso's vector_extract prints Rust f32 Display, the same
    // shortest round-trip spelling the rocks formatter uses.
    assert_eq!(
        rows(
            &mut c,
            "SELECT VECTOR_TO_STRING(STRING_TO_VECTOR('[1.5,2.5]'))"
        )
        .await,
        vec![vec![s("[1.5,2.5]")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT VECTOR_DIM('[1,2]'), VECTOR_DIM(STRING_TO_VECTOR('[1,2,3]'))").await,
        vec![vec![s("2"), s("3")]]
    );
    // VECTOR_DIM(NULL) is NULL on both engines (turso needs the CASE
    // guard: vector32(NULL) would be an engine error).
    assert_eq!(
        rows(&mut c, "SELECT VECTOR_DIM(NULL)").await,
        vec![vec![mysql_async::Value::NULL]]
    );

    // (d) DISTANCE scalars: cosine (identical ~= 1.79e-7, epsilon!),
    // euclidean = sqrt(14), dot = the TRUE inner product (turso's
    // vector_distance_dot negates; the wrapper flips it back per the
    // plan's D5 -- rocks negates instead, see the ledger note below).
    assert!(
        near(
            scalar_f64(&mut c, "SELECT DISTANCE(v, v, 'cosine') FROM vecs WHERE id = 1").await,
            0.0
        ),
        "cosine of identical vectors is ~0, not exactly 0"
    );
    assert!(near(
        scalar_f64(
            &mut c,
            "SELECT DISTANCE(v, '[0,0,0]', 'euclidean') FROM vecs WHERE id = 1"
        )
        .await,
        14f64.sqrt()
    ));
    assert!(near(
        scalar_f64(
            &mut c,
            "SELECT DISTANCE(STRING_TO_VECTOR('[1,2]'), STRING_TO_VECTOR('[3,4]'), 'dot')"
        )
        .await,
        11.0
    ));
    // The uppercase spelling rides cosine. (The 2-arg "default
    // metric" form is parse-time rejected -- 1582 -- on BOTH engines;
    // the renderer's 2-arg arm is defensive only.)
    let d3 = scalar_f64(
        &mut c,
        "SELECT DISTANCE(v, '[1,0,0]', 'cosine') FROM vecs WHERE id = 1",
    )
    .await;
    assert!(near(
        scalar_f64(
            &mut c,
            "SELECT DISTANCE(v, '[1,0,0]', 'COSINE') FROM vecs WHERE id = 1"
        )
        .await,
        d3
    ));
    assert_eq!(
        mysql_server_error(&mut c, "SELECT DISTANCE(v, '[1,0,0]') FROM vecs WHERE id = 1")
            .await
            .code,
        ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT
    );

    // (e) Exact KNN: cosine order 1,5,2,3,4,7,8,6 and euclidean order
    // 2,8,3,5,1,6,4,7 against [1,0,0] (margins >> 1e-6) -- the SAME
    // orderings the rocks engine computes for the same data.
    for (metric, want) in [
        ("cosine", vec!["1", "5", "2", "3", "6", "4", "7", "8"]),
        ("euclidean", vec!["2", "8", "3", "5", "1", "6", "4", "7"]),
    ] {
        let sql = format!(
            "SELECT id FROM vecs ORDER BY DISTANCE(v, '[1,0,0]', '{metric}') LIMIT 8"
        );
        assert_eq!(ids(&rows(&mut c, &sql).await), want, "turso {metric}");
        assert_eq!(
            ids(&rows(&mut r, &sql).await),
            want,
            "rocks {metric} parity"
        );
        // And the top-k head with a query literal wrapped in
        // STRING_TO_VECTOR (the function-composed spelling).
        let sql = format!(
            "SELECT id FROM vecs ORDER BY DISTANCE(v, STRING_TO_VECTOR('[1,0,0]'), '{metric}') LIMIT 3"
        );
        assert_eq!(ids(&rows(&mut c, &sql).await), &want[..3], "turso {metric} k=3");
    }
    // A distance column itself rides the resultset (f32 vs f64
    // accumulation: epsilon, never equality).
    assert!(near(
        scalar_f64(
            &mut c,
            "SELECT DISTANCE(v, '[1,0,0]', 'euclidean') FROM vecs WHERE id = 1"
        )
        .await,
        13f64.sqrt()
    ));
    assert!(near(
        scalar_f64(
            &mut r,
            "SELECT DISTANCE(v, '[1,0,0]', 'euclidean') FROM vecs WHERE id = 1"
        )
        .await,
        13f64.sqrt()
    ));

    // (f) Write-path validation (D6: app-side, rocks-shaped errors).
    // Wrong-dimension vectors are MySQL 1292 with the identical
    // message on both engines; a wrong-domain literal (an INT into a
    // VECTOR column) rides the rocks catch-all: 1136 with the
    // `{Value:?} into {SqlType:?}` shape.
    for sql in [
        "INSERT INTO vecs VALUES (9, '[1,2]')",
        "INSERT INTO vecs VALUES (9, STRING_TO_VECTOR('[1,2]'))",
        "UPDATE vecs SET v = '[1,2]' WHERE id = 1",
    ] {
        let e = mysql_server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_TRUNCATED_WRONG_VALUE, "{sql}: {e:?}");
        let er = mysql_server_error(&mut r, sql).await;
        assert_eq!(er.code, ER_TRUNCATED_WRONG_VALUE, "{sql}: {er:?}");
        assert_eq!(e.message, er.message, "{sql}: message parity with rocks");
    }
    for sql in ["INSERT INTO vecs VALUES (9, 5)"] {
        let e = mysql_server_error(&mut c, sql).await;
        assert_eq!(e.code, 1136, "{sql}: {e:?}");
        let er = mysql_server_error(&mut r, sql).await;
        assert_eq!(er.code, 1136, "{sql}: {er:?}");
        assert_eq!(e.message, er.message, "{sql}: message parity with rocks");
        assert!(
            e.message.contains("cannot store Int(5) into Vector { dim: 3 } column"),
            "{e:?}"
        );
    }
    // The dimension message, verbatim on both engines.
    let e = mysql_server_error(&mut c, "INSERT INTO vecs VALUES (9, '[1,2]')").await;
    assert!(
        e.message.contains("vector has 2 dimensions, column expects 3"),
        "{e:?}"
    );
    // Malformed literals: same shape (parse-level text differs by
    // engine -- code parity only).
    for sql in ["INSERT INTO vecs VALUES (9, 'nope')", "SELECT TO_VECTOR('nope')"] {
        assert_eq!(
            mysql_server_error(&mut c, sql).await.code,
            ER_TRUNCATED_WRONG_VALUE,
            "{sql}"
        );
    }
    // Wrong-dim DISTANCE operands: turso's engine text maps to 1292.
    assert_eq!(
        mysql_server_error(
            &mut c,
            "SELECT DISTANCE(v, '[1,2]', 'cosine') FROM vecs WHERE id = 1"
        )
        .await
        .code,
        ER_TRUNCATED_WRONG_VALUE
    );
    // Arity is parse-time (both engines): 1582.
    assert_eq!(
        mysql_server_error(&mut c, "SELECT VECTOR_DIM(v, v) FROM vecs WHERE id = 1").await.code,
        ER_WRONG_PARAMCOUNT_TO_NATIVE_FCT
    );
    // UPDATE with a well-formed vector round-trips.
    run(&mut c, "UPDATE vecs SET v = '[7,8,9]' WHERE id = 8").await;
    assert_eq!(
        rows(&mut c, "SELECT v FROM vecs WHERE id = 8").await,
        vec![vec![s("[7,8,9]")]]
    );

    // (g) No ANN: CREATE VECTOR INDEX is a loud 1235 with the exact
    // engine verdict (exact KNN only).
    let e = mysql_server_error(&mut c, "CREATE VECTOR INDEX vidx ON vecs (v)").await;
    assert_eq!(e.code, common::mysql::ER_NOT_SUPPORTED_YET, "{e:?}");
    assert_eq!(
        e.message,
        "not supported in this version: CREATE VECTOR INDEX on turso engine \
         (exact KNN only; ANN is rocks-engine only; see COMPAT.sql.md)"
    );

    node.kill_now();
}
