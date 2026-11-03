//! Vector-index physical layer tests: codec round trips, build over
//! visible rows, synchronous DML maintenance, and the ANN probe's
//! recall against the exact scan (identical results on clustered
//! data; approximate recall is a documented deviation, not tested
//! here beyond the parity case).

use super::*;

async fn run(shared: &crate::state::Shared, sql: &str) -> ExecOutcome {
    let mut sess = SqlSession::default();
    execute(shared, &mut sess, parse_statement(sql).unwrap())
        .await
        .unwrap()
}
use crate::sql::exec::{execute, ExecOutcome, SqlSession};

fn rows_of(out: ExecOutcome) -> Vec<Vec<Value>> {
    match out {
        ExecOutcome::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}
use crate::sql::parse::parse_statement;
use crate::sql::storage::codec::{KIND_SQL_VEC_CENTROID, KIND_SQL_VEC_META, KIND_SQL_VEC_POSTING};
use crate::sql::storage::schema::Value;
use crate::store::ops;
use vector::{encode_centroids, encode_meta, encode_postings, Metric, Posting, VectorIndexParams};

#[allow(clippy::too_many_arguments)]
fn params(metric: Metric) -> VectorIndexParams {
    VectorIndexParams {
        metric,
        dim: 2,
        nprobe: 2,
    }
}

#[test]
fn codecs_round_trip() {
    let cal = crate::search::quant::Calibration {
        min: vec![0.0, -1.0],
        scale: vec![0.5, 2.0],
    };
    let meta = encode_meta(&params(Metric::Dot), 3, &cal);
    let (p, k, back) = vector::decode_meta(&meta).unwrap();
    assert_eq!((p, k), (params(Metric::Dot), 3));
    assert_eq!(back.min, cal.min);
    assert_eq!(back.scale, cal.scale);

    let cs = vec![vec![0.1, 2.0], vec![-3.5, 4.0]];
    assert_eq!(
        vector::decode_centroids(&encode_centroids(&cs), 2).unwrap(),
        cs
    );

    let entries = vec![
        Posting {
            pk: b"\x01".to_vec(),
            sq8: vec![1, 2],
        },
        Posting {
            pk: b"\x02".to_vec(),
            sq8: vec![3, 4],
        },
    ];
    assert_eq!(
        vector::decode_postings(&encode_postings(&entries), 2).unwrap(),
        entries
    );
    assert!(vector::decode_postings(b"\x00\x00\x00\x05", 2).is_none());
}

#[tokio::test]
async fn create_builds_and_dml_maintains() {
    let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    run(
        &shared,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2) NOT NULL)",
    )
    .await;
    for id in [1i64, 2, 3, 4] {
        run(
            &shared,
            &format!(
                "INSERT INTO t VALUES ({id}, '[{},{}]')",
                id * 10,
                id * 10 + 1
            ),
        )
        .await;
    }
    run(
        &shared,
        "CREATE VECTOR INDEX tv ON t (v) WITH (METRIC 'euclidean', NPROBE 2)",
    )
    .await;

    let schema = crate::sql::storage::catalog::lookup(&shared, "t")
        .unwrap()
        .unwrap();
    let def = schema.index("tv").expect("vector index in catalog");
    assert_eq!(def.vector.as_ref().unwrap().metric, Metric::Euclidean);
    let col = schema.column_index("v").unwrap() as u32;
    // physical records exist under the dedicated kinds
    assert!(
        ops::get_physical(&shared.store, &vector::meta_key(schema.id, col))
            .unwrap()
            .is_some()
    );
    assert!(
        ops::get_physical(&shared.store, &vector::centroid_key(schema.id, col))
            .unwrap()
            .is_some()
    );
    let mut found = 0;
    ops::for_each_prefix(
        &shared.store,
        &[vector::posting_prefix(schema.id, col)],
        &mut |k, _| {
            if k.contains(&KIND_SQL_VEC_POSTING) {
                found += 1;
            }
            true
        },
    )
    .unwrap();
    assert!(found >= 1, "at least one posting partition");

    // DML maintains synchronously: a new row lands in some partition,
    // a deleted row leaves none. Count entries across partitions.
    let count = |shared: &crate::state::Shared| -> usize {
        let mut total = 0;
        let meta = ops::get_physical(&shared.store, &vector::meta_key(schema.id, col))
            .unwrap()
            .unwrap();
        let (p, k, _) = vector::decode_meta(&meta).unwrap();
        for cid in 0..k {
            if let Some(blob) = ops::get_physical(
                &shared.store,
                &vector::posting_key(schema.id, col, cid as u32),
            )
            .unwrap()
            {
                total += vector::decode_postings(&blob, p.dim as usize)
                    .unwrap()
                    .len();
            }
        }
        total
    };
    run(&shared, "INSERT INTO t VALUES (9, '[100,100]')").await;
    assert_eq!(count(&shared), 5);
    run(&shared, "DELETE FROM t WHERE id = 9").await;
    assert_eq!(count(&shared), 4);

    // DROP INDEX sweeps all three kinds.
    run(&shared, "DROP INDEX tv ON t").await;
    let mut residue = 0;
    ops::for_each_prefix(
        &shared.store,
        &[vector::sweep_start(schema.id, col)],
        &mut |k, _| {
            if k.contains(&KIND_SQL_VEC_META)
                || k.contains(&KIND_SQL_VEC_CENTROID)
                || k.contains(&KIND_SQL_VEC_POSTING)
            {
                residue += 1;
            }
            true
        },
    )
    .unwrap();
    assert_eq!(residue, 0);
}

#[tokio::test]
async fn ann_recall_matches_exact_on_clearly_separated_data() {
    let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    run(
        &shared,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(2) NOT NULL)",
    )
    .await;
    // 3 tight clusters x 5 rows: ANN must return the exact top-3 for
    // a query sitting on one cluster.
    let mut sqls = Vec::new();
    for (c, base) in [(1, 0.0f32), (2, 100.0), (3, 200.0)] {
        for j in 0..5 {
            sqls.push(format!(
                "INSERT INTO t VALUES ({}, '[{:.2},{:.2}]')",
                c * 10 + j,
                base + j as f32 * 0.01,
                base + 2.0 + j as f32 * 0.01
            ));
        }
    }
    for s in sqls {
        run(&shared, &s).await;
    }
    run(&shared, "CREATE VECTOR INDEX tv ON t (v)").await;
    let knn = "SELECT id FROM t ORDER BY DISTANCE(v, '[0,2]', 'euclidean') LIMIT 3";
    // exact path (no index on the metric? index is cosine-default and
    // the query is euclidean -> metric mismatch -> exact scan) AND the
    // ANN path (cosine query) both hand back the first cluster.
    let exact = rows_of(run(&shared, knn).await);
    let ids: Vec<i64> = exact
        .iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            v => panic!("id {v:?}"),
        })
        .collect();
    assert!(ids.iter().all(|i| (10..15).contains(i)), "{ids:?}");

    let ann = "SELECT id FROM t ORDER BY DISTANCE(v, '[0,2]', 'cosine') LIMIT 3";
    let got = rows_of(run(&shared, ann).await);
    let ann_ids: Vec<i64> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            v => panic!("id {v:?}"),
        })
        .collect();
    // both metrics rank the near cluster first; ANN covers all 5
    // members of it in the shortlist (4*3=12 candidates).
    assert!(ann_ids.iter().all(|i| (10..15).contains(i)), "{ann_ids:?}");
}

#[test]
fn parse_matrix_for_create_vector_index() {
    use crate::sql::parse::ast::Statement::*;
    let ok = |sql: &str| match parse_statement(sql).unwrap() {
        CreateVectorIndex {
            table,
            column,
            metric,
            nprobe,
            if_not_exists,
            ..
        } => (table, column, metric, nprobe, if_not_exists),
        other => panic!("not a vector index: {other:?}"),
    };
    assert_eq!(
        ok("CREATE VECTOR INDEX vi ON t (v)"),
        ("t".into(), "v".into(), Metric::Cosine, 4, false)
    );
    assert_eq!(
        ok("create vector index if not exists vi on t (v) with (metric 'dot', nprobe 8)"),
        ("t".into(), "v".into(), Metric::Dot, 8, true)
    );
    assert!(parse_statement("CREATE VECTOR INDEX vi ON t (v) WITH (METRIC 'manhattan')").is_err());
    assert!(parse_statement("CREATE VECTOR INDEX vi ON t (v, w)").is_err());
    assert!(
        parse_statement("CREATE INDEX vi ON t (v)").is_ok(),
        "plain index still parses"
    );
}

#[tokio::test]
async fn ann_euclidean_probe_matches_exact_after_dml() {
    let shared = crate::state::testutil::shared_with(crate::state::testutil::test_config());
    run(
        &shared,
        "CREATE TABLE vec (id BIGINT PRIMARY KEY, v VECTOR(2) NOT NULL)",
    )
    .await;
    let mut sql = String::from("INSERT INTO vec VALUES ");
    for (cid, base) in [(1i64, 0.0f64), (2, 50.0), (3, 100.0)] {
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
    run(&shared, &sql).await;
    run(
        &shared,
        "CREATE VECTOR INDEX vidx ON vec (v) WITH (METRIC 'euclidean', NPROBE 1)",
    )
    .await;
    run(&shared, "INSERT INTO vec VALUES (40, '[0.10,3.20]')").await;
    run(&shared, "DELETE FROM vec WHERE id = 11").await;
    async fn ids(shared: &crate::state::Shared, sql: &str) -> Vec<i64> {
        rows_of(run(shared, sql).await)
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(i) => i,
                ref v => panic!("{v:?}"),
            })
            .collect()
    }
    // ANN (euclidean index, NPROBE 1): near cluster members plus the
    // post-index insert 40 beat every other row.
    let ann = ids(
        &shared,
        "SELECT id FROM vec ORDER BY DISTANCE(v, '[0,3]', 'euclidean') LIMIT 3",
    )
    .await;
    assert_eq!(ann, vec![10, 40, 12]);
    // metric mismatch falls back to the exact scan with same winners.
    let exact = ids(
        &shared,
        "SELECT id FROM vec ORDER BY DISTANCE(v, '[0,3]', 'cosine') LIMIT 3",
    )
    .await;
    assert_eq!(exact, vec![10, 40, 12]);
    // drop the index: the exact path answers identically.
    run(&shared, "DROP INDEX vidx ON vec").await;
    let after = ids(
        &shared,
        "SELECT id FROM vec ORDER BY DISTANCE(v, '[0,3]', 'euclidean') LIMIT 3",
    )
    .await;
    assert_eq!(after, vec![10, 40, 12]);
}

/// C-1 regression: two concurrent DML statements both read a posting
/// partition before either writes -- without per-index serialization the
/// second write drops the first's appended entry (silent ANN recall
/// loss). 8 tasks x 5 inserts must leave exactly seed+40 postings.
#[tokio::test]
async fn concurrent_dml_keeps_every_posting() {
    let shared = std::sync::Arc::new(crate::state::testutil::shared_with(
        crate::state::testutil::test_config(),
    ));
    run(
        &shared,
        "CREATE TABLE vec (id BIGINT PRIMARY KEY, v VECTOR(2) NOT NULL)",
    )
    .await;
    for id in 1..=8i64 {
        run(
            &shared,
            &format!("INSERT INTO vec VALUES ({id}, '[{id},{id}]')"),
        )
        .await;
    }
    run(&shared, "CREATE VECTOR INDEX tv ON vec (v)").await;

    let mut handles = Vec::new();
    for t in 0..8i64 {
        let sh = std::sync::Arc::clone(&shared);
        handles.push(tokio::spawn(async move {
            for j in 0..5i64 {
                let id = 100 + t * 10 + j;
                let mut sess = crate::sql::exec::SqlSession::default();
                crate::sql::exec::execute(
                    &sh,
                    &mut sess,
                    parse_statement(&format!("INSERT INTO vec VALUES ({id}, '[{id},{id}]')"))
                        .unwrap(),
                )
                .await
                .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let schema = crate::sql::storage::catalog::lookup(&shared, "vec")
        .unwrap()
        .unwrap();
    let col = schema.column_index("v").unwrap() as u32;
    let meta = ops::get_physical(&shared.store, &vector::meta_key(schema.id, col))
        .unwrap()
        .unwrap();
    let (p, k, _) = vector::decode_meta(&meta).unwrap();
    let mut total = 0;
    for cid in 0..k {
        if let Some(blob) = ops::get_physical(
            &shared.store,
            &vector::posting_key(schema.id, col, cid as u32),
        )
        .unwrap()
        {
            total += vector::decode_postings(&blob, p.dim as usize)
                .unwrap()
                .len();
        }
    }
    assert_eq!(total, 48, "posting entries must equal live rows");
}
