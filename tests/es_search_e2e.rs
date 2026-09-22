//! Process-level e2e for the ES `_search` DSL matrix (match/term/terms/
//! range/bool, sort, from/size, _source shaping, knn) plus the KEY
//! interop proof: an index created through RESP FT.* is queried and
//! extended over ES HTTP, and docs written over ES are found by
//! FT.SEARCH -- both fronts sit on ONE search kernel.

mod common;
mod es_common;

use es_common::{http, json_body, spawn_es_ready};

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{cmd_one_shot, TOKEN};

const MAPPINGS: &str = r#"{"mappings":{"properties":{
"title":{"type":"text"},"tag":{"type":"keyword"},
"price":{"type":"long"},"vec":{"type":"dense_vector","dims":2}}}}"#;

/// The 5-doc corpus: CJK + english titles, case-varied tags, prices
/// 10..50, vectors spread around the unit box.
const DOCS: [(&str, &str); 5] = [
    (
        "d1",
        r#"{"title":"Redis 快速入门","tag":"Redis","price":10,"vec":[0,0]}"#,
    ),
    (
        "d2",
        r#"{"title":"redis in action","tag":"redis","price":20,"vec":[1,1]}"#,
    ),
    (
        "d3",
        r#"{"title":"向量检索实战","tag":"vector","price":30,"vec":[2,2]}"#,
    ),
    (
        "d4",
        r#"{"title":"redis cluster protocol guide","tag":"Redis","price":40,"vec":[3,3]}"#,
    ),
    (
        "d5",
        r#"{"title":"中文分词测试","tag":"mixed","price":50,"vec":[1,3]}"#,
    ),
];

/// POST /{index}/_search; panics on non-200 so matrix cases read clean.
async fn search(es: &str, body: &str) -> serde_json::Value {
    let (status, out) = http(es, "POST", "/books/_search", Some(body)).await;
    assert_eq!(status, 200, "{body} -> {out}");
    json_body(&out)
}

fn ids(v: &serde_json::Value) -> Vec<String> {
    v["hits"]["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// One AUTHed RESP command on the node's resp port, as text.
async fn resp_text(addr: &str, args: &[&[u8]]) -> String {
    String::from_utf8_lossy(&cmd_one_shot(addr, TOKEN, args).await).into_owned()
}

/// Hand-rolled RESP2 array call returning the FULL raw reply (AUTH +OK
/// pipelined before the command): `cmd_one_shot`'s line reader stops at
/// the first line, but FT.SEARCH answers with an array whose total and
/// hit ids live deeper in the frame. Reads until a 300ms idle gap.
async fn resp_call(addr: &str, args: &[&[u8]]) -> String {
    let mut sock = TcpStream::connect(addr).await.expect("connect resp");
    let mut req = format!("*2\r\n$4\r\nAUTH\r\n${}\r\n{TOKEN}\r\n", TOKEN.len()).into_bytes();
    let head = format!("*{}\r\n", args.len());
    req.extend_from_slice(head.as_bytes());
    for a in args {
        req.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        req.extend_from_slice(a);
        req.extend_from_slice(b"\r\n");
    }
    sock.write_all(&req).await.expect("write");
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let mut chunk = [0u8; 4096];
        match tokio::time::timeout(Duration::from_millis(300), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
            Ok(Err(_)) => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn dsl_matrix() {
    let (_node, es) = spawn_es_ready("dsl").await;
    let (status, _) = http(&es, "PUT", "/books", Some(MAPPINGS)).await;
    assert_eq!(status, 200);
    for (id, doc) in DOCS {
        let path = format!("/books/_doc/{id}");
        let (status, body) = http(&es, "PUT", &path, Some(doc)).await;
        assert_eq!(status, 201, "{id}: {body}");
    }

    // match_all + match (default or: union of the term postings).
    assert_eq!(
        search(&es, r#"{"query":{"match_all":{}}}"#).await["hits"]["total"]["value"],
        5
    );
    let v = search(&es, r#"{"query":{"match":{"title":"redis"}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}");
    let top = v["hits"]["hits"][0]["_source"]["title"].as_str().unwrap();
    assert!(top.to_lowercase().contains("redis"), "top: {top}");

    // match operator and: both terms in one doc only.
    let v = search(
        &es,
        r#"{"query":{"match":{"title":{"query":"redis action","operator":"and"}}}}"#,
    )
    .await;
    assert_eq!(ids(&v), vec!["d2"], "{v}");

    // match on a keyword field: the QUERY is analyzed ("Redis" ->
    // "redis") while keyword terms keep exact bytes, so only the
    // lowercase-tagged doc matches (case variance is observable).
    let v = search(&es, r#"{"query":{"match":{"tag":"Redis"}}}"#).await;
    assert_eq!(ids(&v), vec!["d2"], "{v}");

    // Chinese: query term survives segmentation on both sides.
    let v = search(&es, r#"{"query":{"match":{"title":"分词"}}}"#).await;
    assert_eq!(ids(&v), vec!["d5"], "{v}");

    // term is case-sensitive on keyword fields.
    let v = search(&es, r#"{"query":{"term":{"tag":"Redis"}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 2, "{v}");
    let v = search(&es, r#"{"query":{"term":{"tag":"redis"}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 1, "{v}");
    // terms = multi-value OR.
    let v = search(&es, r#"{"query":{"terms":{"tag":["vector","mixed"]}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 2, "{v}");

    // range combos; range on a text field matches nothing, no error.
    assert_eq!(
        search(&es, r#"{"query":{"range":{"price":{"gte":30}}}}"#).await["hits"]["total"]["value"],
        3
    );
    assert_eq!(
        search(&es, r#"{"query":{"range":{"price":{"lte":20}}}}"#).await["hits"]["total"]["value"],
        2
    );
    assert_eq!(
        search(&es, r#"{"query":{"range":{"price":{"gte":20,"lte":40}}}}"#).await["hits"]["total"]
            ["value"],
        3
    );
    // Range on a non-numeric FIELD matches nothing (0 hits, no error);
    // string bounds are rejected at parse time regardless of field.
    assert_eq!(
        search(&es, r#"{"query":{"range":{"title":{"gte":0}}}}"#).await["hits"]["total"]["value"],
        0
    );
    let (status, body) = http(
        &es,
        "POST",
        "/books/_search",
        Some(r#"{"query":{"range":{"title":{"gte":"a"}}}}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // bool: must+filter / must_not / should-only = OR / should optional.
    let v = search(
        &es,
        r#"{"query":{"bool":{"must":[{"match":{"title":"redis"}}],"filter":[{"range":{"price":{"gte":30}}}]}}}"#,
    ).await;
    assert_eq!(ids(&v), vec!["d4"], "{v}");
    let v = search(
        &es,
        r#"{"query":{"bool":{"must":{"match_all":{}},"must_not":{"term":{"tag":"vector"}}}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 4, "{v}");
    let v = search(
        &es,
        r#"{"query":{"bool":{"should":[{"term":{"tag":"Redis"}},{"term":{"tag":"vector"}}]}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}");
    let v = search(
        &es,
        r#"{"query":{"bool":{"must":[{"match":{"title":"redis"}}],"should":[{"term":{"tag":"vector"}}]}}}"#,
    ).await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}");

    // sort: string + object forms, _doc, missing field last.
    let v = search(&es, r#"{"sort":["price"],"query":{"match_all":{}}}"#).await;
    assert_eq!(v["hits"]["hits"][0]["_id"], "d1", "{v}");
    let v = search(
        &es,
        r#"{"sort":[{"price":{"order":"desc"}}],"query":{"match_all":{}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["hits"][0]["_id"], "d5", "{v}");
    let v = search(&es, r#"{"sort":["_doc"],"query":{"match_all":{}}}"#).await;
    assert_eq!(v["hits"]["hits"][0]["_id"], "d1", "{v}");
    let (status, _) = http(
        &es,
        "PUT",
        "/books/_doc/d6",
        Some(r#"{"title":"no price","tag":"x"}"#),
    )
    .await;
    assert_eq!(status, 201);
    let v = search(
        &es,
        r#"{"sort":["price"],"size":6,"query":{"match_all":{}}}"#,
    )
    .await;
    let hits = v["hits"]["hits"].as_array().unwrap();
    assert_eq!(hits[0]["_id"], "d1");
    assert_eq!(
        hits[5]["_id"], "d6",
        "missing sort value must sort last: {v}"
    );
    let (status, _) = http(&es, "DELETE", "/books/_doc/d6", None).await;
    assert_eq!(status, 200);

    // from/size paging keeps the pre-window total; the 10k window cap.
    let v = search(&es, r#"{"from":2,"size":2,"query":{"match_all":{}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 5);
    assert_eq!(v["hits"]["hits"].as_array().unwrap().len(), 2);
    let (status, body) = http(
        &es,
        "POST",
        "/books/_search",
        Some(r#"{"from":9999,"size":2,"query":{"match_all":{}}}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // _source shaping: off (null placeholder) / includes / excludes.
    let v = search(&es, r#"{"_source":false,"query":{"match_all":{}}}"#).await;
    assert!(v["hits"]["hits"][0]["_source"].is_null(), "{v}");
    let v = search(&es, r#"{"_source":["title"],"query":{"match_all":{}}}"#).await;
    let src = &v["hits"]["hits"][0]["_source"];
    assert!(
        src.get("title").is_some() && src.get("price").is_none(),
        "{v}"
    );
    let v = search(
        &es,
        r#"{"_source":{"excludes":["vec"]},"query":{"match_all":{}}}"#,
    )
    .await;
    let src = &v["hits"]["hits"][0]["_source"];
    assert!(
        src.get("vec").is_none() && src.get("title").is_some(),
        "{v}"
    );

    // knn: score 1/(1+L2) <= 1, nearest doc first; filter restricts.
    let v = search(
        &es,
        r#"{"knn":{"field":"vec","query_vector":[0.1,0.1],"k":2,"num_candidates":50}}"#,
    )
    .await;
    assert_eq!(v["hits"]["hits"][0]["_id"], "d1", "{v}");
    let score = v["hits"]["hits"][0]["_score"].as_f64().unwrap();
    assert!(score > 0.0 && score <= 1.0, "{v}");
    let v = search(
        &es,
        r#"{"knn":{"field":"vec","query_vector":[3,3],"k":2,"num_candidates":50,"filter":{"term":{"tag":"Redis"}}}}"#,
    ).await;
    assert_eq!(v["hits"]["hits"][0]["_id"], "d4", "{v}");

    // knn against an index with no VECTOR field: error envelope, no panic.
    let (status, _) = http(
        &es,
        "PUT",
        "/nov",
        Some(r#"{"mappings":{"properties":{"t":{"type":"text"}}}}"#),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = http(
        &es,
        "POST",
        "/nov/_search",
        Some(r#"{"knn":{"field":"vec","query_vector":[0,0],"k":1,"num_candidates":10}}"#),
    )
    .await;
    assert!(status == 400 || status == 500, "{status} {body}");
    assert!(json_body(&body)["error"]["type"].is_string(), "{body}");

    // _count honors a query.
    let (status, body) = http(
        &es,
        "POST",
        "/books/_count",
        Some(r#"{"query":{"term":{"tag":"Redis"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["count"], 2);
}

#[tokio::test]
async fn resp_es_interop_one_kernel() {
    let (_node, es) = spawn_es_ready("interop").await;
    let resp_addr = _node.resp.clone();

    // RESP creates + feeds the index; ES reads its mappings and docs.
    let r = resp_text(
        &resp_addr,
        &[
            b"ft.create",
            b"ridx",
            b"SCHEMA",
            b"body",
            b"TEXT",
            b"tag",
            b"KEYWORD",
            b"price",
            b"NUMERIC",
        ],
    )
    .await;
    assert!(r.starts_with("+OK"), "ft.create: {r}");
    let r = resp_text(
        &resp_addr,
        &[
            b"ft.add",
            b"ridx",
            b"d1",
            br#"{"body":"hello world","tag":"x","price":5}"#,
        ],
    )
    .await;
    assert!(r.starts_with(":1"), "ft.add: {r}");

    let (status, body) = http(&es, "GET", "/ridx", None).await;
    assert_eq!(status, 200, "{body}");
    let props = &json_body(&body)["ridx"]["mappings"]["properties"];
    assert_eq!(props["body"]["type"], "text");
    assert_eq!(props["tag"]["type"], "keyword");
    assert_eq!(props["price"]["type"], "long");

    let (status, body) = http(
        &es,
        "POST",
        "/ridx/_search",
        Some(r#"{"query":{"term":{"tag":"x"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let v = json_body(&body);
    assert_eq!(v["hits"]["total"]["value"], 1, "{v}");
    assert_eq!(v["hits"]["hits"][0]["_id"], "d1");
    assert_eq!(v["hits"]["hits"][0]["_source"]["price"], 5);

    // ES writes a doc; RESP finds it (FT.SEARCH total covers both).
    let (status, body) = http(
        &es,
        "PUT",
        "/ridx/_doc/d2",
        Some(r#"{"body":"second doc","tag":"y","price":7}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let r = resp_call(&resp_addr, &[b"ft.search", b"ridx", b"@body:second"]).await;
    assert!(r.contains(":1\r\n"), "ft.search second: {r:?}");
    assert!(r.contains("d2"), "hit id missing: {r:?}");
    let r = resp_call(&resp_addr, &[b"ft.search", b"ridx", b"*"]).await;
    assert!(r.contains(":2\r\n"), "ft.search all: {r:?}");
}
