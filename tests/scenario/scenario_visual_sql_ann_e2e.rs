//! Multi-store scenario #118 -- visual-similarity ANN on the SQL plane,
//! cross-checked against the ES knn face: a 20-row `VECTOR(4)` table
//! answers `ORDER BY DISTANCE(...)` under three metrics (euclidean /
//! cosine / dot -- each with its OWN neighbour ranking), the metadata
//! spells the type as `vector(4)`, the vector functions round-trip
//! (`STRING_TO_VECTOR` = `TO_VECTOR`, `VECTOR_DIM`, `VECTOR_TO_STRING`),
//! a `CREATE VECTOR INDEX ... WITH (METRIC 'euclidean', NPROBE 1)`
//! shows up as a BTREE row in `SHOW INDEX` and keeps the KNN answer
//! identical (ANN == exact on this corpus), and the ES mirror of the
//! same corpus returns the SAME top-5 ids. One REAL node, MySQL + ES.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run, s};
use scenario_common::{front_world, http, json_of, FrontSpec};
use std::time::Duration;

const DIM: usize = 4;
/// Query vector of every KNN probe (deliberately NOT one of the rows).
const QUERY: [f64; 4] = [0.0, 0.0, 2.0, 5.0];

/// Row `i`'s embedding: four small integers, distinct per row.
fn vec_of(i: usize) -> [f64; DIM] {
    let n = (i + 1) as f64;
    [(n * 7.0) % 11.0, (n * 3.0) % 5.0, n % 3.0, (n * 5.0) % 7.0]
}

/// SQL literal of a vector (`[a,b,c,d]`).
fn lit(v: &[f64]) -> String {
    let cells: Vec<String> = v.iter().map(|x| format!("{}", *x as i64)).collect();
    format!("[{}]", cells.join(","))
}

/// Metric distance of the SQL `DISTANCE(a, b, '<metric>')` call.
fn dist(metric: &str, a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    match metric {
        "euclidean" => a
            .iter()
            .zip(b)
            .map(|(x, y)| (x - y) * (x - y))
            .sum::<f64>()
            .sqrt(),
        "cosine" => {
            let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
            let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
            1.0 - dot / (na * nb)
        }
        // `dot` ranks by NEGATED inner product (bigger product first).
        _ => -dot,
    }
}

/// Oracle: the ids of the `k` nearest rows under `metric`, nearest
/// first (a tie inside the window would make the order ambiguous, so
/// the caller asserts the window is tie-free).
fn knn_ids(metric: &str, k: usize) -> Vec<i64> {
    let mut ranked: Vec<(f64, i64)> = (0..20)
        .map(|i| (dist(metric, &vec_of(i), &QUERY), i as i64 + 1))
        .collect();
    ranked.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then(a.1.cmp(&b.1)));
    ranked.into_iter().take(k).map(|(_, id)| id).collect()
}

/// Text of a wire cell.
fn txt(v: &mysql_async::Value) -> String {
    match v {
        mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn vector_knn_three_metrics_and_the_es_mirror() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, _resp) = front_world("vec118", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // a) The table: metadata spells the type `vector(4)`.
    ddl(
        &mut c,
        "CREATE TABLE vec (id BIGINT PRIMARY KEY, v VECTOR(4) NOT NULL)",
    )
    .await;
    let described = rows(&mut c, "DESCRIBE vec").await;
    assert_eq!(described[1][0], s("v"), "describe: {described:?}");
    let ty = txt(&described[1][1]).to_ascii_lowercase();
    assert!(ty.contains("vector(4)"), "type name: {described:?}");

    // b) The hot block: 20 single-row INSERTs + 10 KNN queries.
    let t = scenario_common::perf::now();
    for i in 0..20 {
        run(
            &mut c,
            &format!("INSERT INTO vec VALUES ({}, '{}')", i + 1, lit(&vec_of(i))),
        )
        .await;
    }
    for metric in ["euclidean", "cosine", "dot", "euclidean", "dot"] {
        for limit in [5, 3] {
            rows(
                &mut c,
                &format!(
                    "SELECT id FROM vec ORDER BY DISTANCE(v, '{}', '{metric}') LIMIT {limit}",
                    lit(&QUERY)
                ),
            )
            .await;
        }
    }
    scenario_common::perf::within("vec118/20-insert+10-knn", t, Duration::from_secs(3));

    // c) The vector functions: STRING_TO_VECTOR == TO_VECTOR, dims
    //    read back as 4, and VECTOR_TO_STRING renders the stored row.
    assert_eq!(
        rows(
            &mut c,
            "SELECT STRING_TO_VECTOR('[1,2,3,4]') = TO_VECTOR('[1,2,3,4]'), \
             VECTOR_DIM(STRING_TO_VECTOR('[1,2,3,4]'))"
        )
        .await,
        vec![vec![s("1"), s("4")]],
        "string_to_vector"
    );
    assert_eq!(
        rows(&mut c, "SELECT VECTOR_DIM(v) FROM vec WHERE id = 1").await,
        vec![vec![s("4")]],
        "vector_dim"
    );
    assert_eq!(
        rows(&mut c, "SELECT VECTOR_TO_STRING(v) FROM vec WHERE id = 1").await,
        vec![vec![s(&lit(&vec_of(0)))]],
        "vector_to_string"
    );

    // d) Each metric ranks the corpus its OWN way -- and each ranking
    //    is the oracle's (windows are tie-free, so order is exact).
    for metric in ["euclidean", "cosine", "dot"] {
        let mut ranked: Vec<f64> = (0..20).map(|i| dist(metric, &vec_of(i), &QUERY)).collect();
        ranked.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(
            ranked[4] < ranked[5],
            "{metric} window is tie-free: {ranked:?}"
        );
        assert_eq!(
            rows(
                &mut c,
                &format!(
                    "SELECT id FROM vec ORDER BY DISTANCE(v, '{}', '{metric}') LIMIT 5",
                    lit(&QUERY)
                )
            )
            .await,
            knn_ids(metric, 5)
                .iter()
                .map(|id| vec![s(&id.to_string())])
                .collect::<Vec<_>>(),
            "{metric} top-5"
        );
    }
    // the three metrics really do disagree (this is not one ranking
    // spelled three ways)
    assert_ne!(knn_ids("euclidean", 5), knn_ids("cosine", 5));
    assert_ne!(knn_ids("euclidean", 5), knn_ids("dot", 5));
    // distance cells: the euclidean ranking carries its own distances
    assert_eq!(
        rows(
            &mut c,
            &format!(
                "SELECT ROUND(DISTANCE(v, '{}', 'euclidean'), 4) FROM vec WHERE id = 5",
                lit(&QUERY)
            )
        )
        .await,
        vec![vec![s(&format!(
            "{:.4}",
            dist("euclidean", &vec_of(4), &QUERY)
        ))]],
        "euclidean distance cell"
    );

    // e) The vector index: `WITH (METRIC 'euclidean', NPROBE 1)` is
    //    accepted, SHOW INDEX reports it as a BTREE row on column v,
    //    and the indexed (ANN) path answers exactly like the exact one.
    ddl(
        &mut c,
        "CREATE VECTOR INDEX vidx ON vec (v) WITH (METRIC 'euclidean', NPROBE 1)",
    )
    .await;
    let shown = rows(&mut c, "SHOW INDEX FROM vec").await;
    let vidx = shown
        .iter()
        .find(|r| txt(&r[2]) == "vidx")
        .unwrap_or_else(|| panic!("vidx row missing: {shown:?}"));
    assert_eq!(txt(&vidx[5]), "BTREE", "index type: {shown:?}");
    assert_eq!(txt(&vidx[4]), "v", "indexed column: {shown:?}");
    assert!(
        shown.iter().any(|r| txt(&r[2]) == "PRIMARY"),
        "pk row still listed: {shown:?}"
    );
    assert_eq!(
        rows(
            &mut c,
            &format!(
                "SELECT id FROM vec ORDER BY DISTANCE(v, '{}', 'euclidean') LIMIT 5",
                lit(&QUERY)
            )
        )
        .await,
        knn_ids("euclidean", 5)
            .iter()
            .map(|id| vec![s(&id.to_string())])
            .collect::<Vec<_>>(),
        "ANN top-5 == exact top-5"
    );

    // f) The ES mirror: same 20 embeddings in a dims-4 dense_vector
    //    field; its knn face returns the SAME top-5 ids, nearest first.
    let (status, body) = http(
        &es,
        "PUT",
        "/visual118",
        Some(
            r#"{"mappings":{"properties":{
            "v":{"type":"dense_vector","dims":4}}}}"#,
        ),
    )
    .await;
    assert_eq!(status, 200, "create visual118: {body}");
    let mut ndjson = String::new();
    for i in 0..20 {
        ndjson.push_str(&format!(
            "{{\"index\":{{\"_index\":\"visual118\",\"_id\":\"{}\"}}}}\n{{\"v\":{}}}\n",
            i + 1,
            lit(&vec_of(i))
        ));
    }
    let (status, body) = http(&es, "POST", "/_bulk", Some(&ndjson)).await;
    assert_eq!(status, 200, "bulk visual118: {body}");
    assert_eq!(json_of(&body)["errors"], false, "{body}");
    let (status, body) = http(
        &es,
        "POST",
        "/visual118/_search",
        Some(&format!(
            r#"{{"knn":{{"field":"v","query_vector":{},"k":5,"num_candidates":50}}}}"#,
            lit(&QUERY)
        )),
    )
    .await;
    assert_eq!(status, 200, "es knn: {body}");
    let v = json_of(&body);
    let hits = v["hits"]["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 5, "{body}");
    for (h, want) in hits.iter().zip(knn_ids("euclidean", 5)) {
        assert_eq!(h["_id"], want.to_string(), "es knn order: {body}");
        let score = h["_score"].as_f64().expect("score");
        assert!(score > 0.0 && score <= 1.0, "score {score}: {body}");
    }
    scenario_common::perf::scenario("vec118", t0);
}
