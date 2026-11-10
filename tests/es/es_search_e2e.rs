//! Process-level e2e for the ES `_search` DSL matrix (match/term/terms/
//! range/bool, sort, from/size, _source shaping, knn) plus the KEY
//! interop proof: an index created through RESP FT.* is queried and
//! extended over ES HTTP, and docs written over ES are found by
//! FT.SEARCH -- both fronts sit on ONE search kernel.

use crate::common;
use crate::es_common;

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

    // v2 operators cross the wire BOTH ways over the SAME kernel:
    // RESP writes a phrase, ES match_phrase finds it adjacently.
    let r = resp_text(
        &resp_addr,
        &[
            b"ft.add",
            b"ridx",
            b"d3",
            br#"{"body":"quick brown fox jumps","tag":"z","price":9}"#,
        ],
    )
    .await;
    assert!(r.starts_with(":1"), "ft.add d3: {r}");
    let (status, body) = http(
        &es,
        "POST",
        "/ridx/_search",
        Some(r#"{"query":{"match_phrase":{"body":"brown fox"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&json_body(&body)), vec!["d3"], "{body}");
    // ES writes, RESP phrase + prefix find it (v2 positions present
    // on both write paths, so phrase works cross-frontend).
    let (status, body) = http(
        &es,
        "PUT",
        "/ridx/_doc/d4",
        Some(r#"{"body":"deep dark depth","tag":"w","price":11}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let r = resp_call(
        &resp_addr,
        &[b"ft.search", b"ridx", br#"@body:"dark depth""#],
    )
    .await;
    assert!(r.contains("d4"), "ft.search phrase d4: {r:?}");
    let r = resp_call(&resp_addr, &[b"ft.search", b"ridx", b"@body:sec*"]).await;
    assert!(r.contains("d2"), "ft.search prefix d2: {r:?}");
    let (status, body) = http(
        &es,
        "POST",
        "/ridx/_search",
        Some(r#"{"query":{"prefix":{"body":"deep"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&json_body(&body)), vec!["d4"], "{body}");
}

/// v2 DSL clauses: match_phrase / prefix / query_string /
/// minimum_should_match / term object form / numeric doc-value sort.
/// All of them run over the SAME kernel records as FT.SEARCH.
#[tokio::test]
async fn dsl_v2_phrase_prefix_qs_msm() {
    let (_node, es) = spawn_es_ready("dslv2").await;
    let (status, _) = http(&es, "PUT", "/books", Some(MAPPINGS)).await;
    assert_eq!(status, 200);
    for (id, doc) in DOCS {
        let path = format!("/books/_doc/{id}");
        let (status, body) = http(&es, "PUT", &path, Some(doc)).await;
        assert_eq!(status, 201, "{id}: {body}");
    }

    // match_phrase: ordered adjacency (positions), both object and
    // shorthand forms; reversed order matches nothing.
    let v = search(
        &es,
        r#"{"query":{"match_phrase":{"title":"cluster protocol"}}}"#,
    )
    .await;
    assert_eq!(ids(&v), vec!["d4"], "{v}");
    let v = search(
        &es,
        r#"{"query":{"match_phrase":{"title":{"query":"protocol cluster"}}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 0, "{v}");
    // CJK phrase rides the bigram ordinals.
    let v = search(&es, r#"{"query":{"match_phrase":{"title":"中文分词"}}}"#).await;
    assert_eq!(ids(&v), vec!["d5"], "{v}");
    // empty analysis matches nothing (no error).
    assert_eq!(
        search(&es, r#"{"query":{"match_phrase":{"title":"!!!"}}}"#).await["hits"]["total"]
            ["value"],
        0
    );

    // prefix = term disjunction over the posting keys.
    let v = search(&es, r#"{"query":{"prefix":{"title":"red"}}}"#).await;
    let mut got = ids(&v);
    got.sort();
    assert_eq!(got, vec!["d1", "d2", "d4"], "{v}");
    let v = search(&es, r#"{"query":{"prefix":{"title":"向量"}}}"#).await;
    assert_eq!(ids(&v), vec!["d3"], "{v}");
    let v = search(&es, r#"{"query":{"prefix":{"title":"zzz"}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 0, "{v}");

    // query_string shares the FT.SEARCH grammar (strict implicit AND,
    // bare field scopes, quoted phrases, | union, `*`).
    let v = search(
        &es,
        r#"{"query":{"query_string":{"query":"redis AND cluster"}}}"#,
    )
    .await;
    assert_eq!(ids(&v), vec!["d4"], "{v}");
    let v = search(
        &es,
        r#"{"query":{"query_string":{"query":"title:redis | title:向量"}}}"#,
    )
    .await;
    let mut got = ids(&v);
    got.sort();
    assert_eq!(got, vec!["d1", "d2", "d3", "d4"], "{v}");
    let v = search(
        &es,
        r#"{"query":{"query_string":{"query":"title:\"cluster protocol\""}}}"#,
    )
    .await;
    assert_eq!(ids(&v), vec!["d4"], "{v}");
    let v = search(&es, r#"{"query":{"query_string":{"query":"*"}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 5, "{v}");
    // query_string 桥接 `price:[10 20]`：共享 parser 的 NumRange
    // （doc-values 列扫描，常量分）；非法区间是 400
    let v = search(
        &es,
        r#"{"query":{"query_string":{"query":"price:[10 20] OR tag:vector"}}}"#,
    )
    .await;
    let mut got = ids(&v);
    got.sort();
    assert_eq!(got, vec!["d1", "d2", "d3"], "{v}");
    let (status, body) = http(
        &es,
        "POST",
        "/books/_search",
        Some(r#"{"query":{"query_string":{"query":"price:[10]"}}}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    // unknown extra keys are tolerated; missing query is a 400.
    let v = search(
        &es,
        r#"{"query":{"query_string":{"query":"redis","default_field":"title"}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}");
    let (status, body) = http(
        &es,
        "POST",
        "/books/_search",
        Some(r#"{"query":{"query_string":{}}}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // minimum_should_match, should-only: count and percent forms.
    let shoulds = r#"{"query":{"bool":{"should":[{"term":{"tag":"Redis"}},{"term":{"tag":"vector"}}],"minimum_should_match":"#;
    let v = search(&es, &format!(r#"{shoulds}1}}}}}}"#)).await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}"); // default union
    let v = search(&es, &format!(r#"{shoulds}2}}}}}}"#)).await;
    assert_eq!(v["hits"]["total"]["value"], 0, "{v}"); // needs BOTH tags
    let v = search(&es, &format!(r#"{shoulds}"50%"}}}}}}"#)).await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}"); // round(2*50%)=1
    let v = search(&es, &format!(r#"{shoulds}"100%"}}}}}}"#)).await;
    assert_eq!(v["hits"]["total"]["value"], 0, "{v}"); // round(2*100%)=2
                                                       // negative/combination forms are a 400.
    for bad in ["\"-1\"", "\"2<-25%\""] {
        let (status, body) = http(
            &es,
            "POST",
            "/books/_search",
            Some(&format!(r#"{shoulds}{bad}}}}}}}"#)),
        )
        .await;
        assert_eq!(status, 400, "{bad}: {body}");
    }

    // msm beside must: explicit gate instead of additive should.
    let v = search(
        &es,
        r#"{"query":{"bool":{"must":[{"match":{"title":"redis"}}],"should":[{"term":{"tag":"vector"}}]}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 3, "{v}"); // additive default
    let v = search(
        &es,
        r#"{"query":{"bool":{"must":[{"match":{"title":"redis"}}],"should":[{"term":{"tag":"vector"}}],"minimum_should_match":1}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 0, "{v}"); // no redis doc has tag vector

    // term object form: {"value": ..} (+ tolerated boost).
    let v = search(
        &es,
        r#"{"query":{"term":{"tag":{"value":"Redis","boost":2}}}}"#,
    )
    .await;
    assert_eq!(v["hits"]["total"]["value"], 2, "{v}");
    let v = search(&es, r#"{"query":{"term":{"price":{"value":30}}}}"#).await;
    assert_eq!(ids(&v), vec!["d3"], "{v}");
    let (status, body) = http(
        &es,
        "POST",
        "/books/_search",
        Some(r#"{"query":{"term":{"tag":{"boost":1}}}}"#),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    // numeric doc-value sort composes with the new clauses.
    let v = search(
        &es,
        r#"{"sort":[{"price":"desc"}],"query":{"prefix":{"title":"red"}}}"#,
    )
    .await;
    assert_eq!(ids(&v), vec!["d4", "d2", "d1"], "{v}");
    let v = search(
        &es,
        r#"{"sort":["price"],"query":{"query_string":{"query":"redis -tag:vector"}}}"#,
    )
    .await;
    assert_eq!(ids(&v), vec!["d1", "d2", "d4"], "{v}");
}

/// WP3 停用词互操作：mapping 建的索引（默认无停用词）match 不受影响；
/// FT.CREATE 建带停用词的索引 + ES 写文档后，match / query_string 的
/// 停用词在查询侧被过滤（同一内核，ES 写路径同样过滤建索引）。
#[tokio::test]
async fn es_stopwords_across_fronts() {
    let (_node, es) = spawn_es_ready("stopw").await;
    let resp_addr = _node.resp.clone();

    // mapping 创建（默认无停用词）：普通词照常命中。
    let (status, body) = http(
        &es,
        "PUT",
        "/plain",
        Some(r#"{"mappings":{"properties":{"t":{"type":"text"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = http(
        &es,
        "PUT",
        "/plain/_doc/1",
        Some(r#"{"t":"the quick fox"}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let (status, body) = http(
        &es,
        "POST",
        "/plain/_search",
        Some(r#"{"query":{"match":{"t":"quick"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["hits"]["total"]["value"], 1, "{body}");

    // RESP 建带停用词的索引；ES 写文档（索引侧过滤，位置紧凑）。
    let r = resp_text(
        &resp_addr,
        &[
            b"ft.create",
            b"sw",
            b"STOPWORDS",
            b"1",
            b"the",
            b"SCHEMA",
            b"body",
            b"TEXT",
        ],
    )
    .await;
    assert!(r.starts_with("+OK"), "ft.create: {r}");
    let (status, body) = http(
        &es,
        "PUT",
        "/sw/_doc/1",
        Some(r#"{"body":"the quick the fox"}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    // match 停用词 -> 空；普通词不受影响。
    let (status, body) = http(
        &es,
        "POST",
        "/sw/_search",
        Some(r#"{"query":{"match":{"body":"the"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["hits"]["total"]["value"], 0, "{body}");
    let (status, body) = http(
        &es,
        "POST",
        "/sw/_search",
        Some(r#"{"query":{"match":{"body":"quick"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["hits"]["total"]["value"], 1, "{body}");

    // query_string 桥（共享 eval_tree）：短语里的停用词被滤掉，剩下的
    // quick/fox 因索引侧位置紧凑而相邻命中。
    let (status, body) = http(
        &es,
        "POST",
        "/sw/_search",
        Some(r#"{"query":{"query_string":{"query":"body:\"quick the fox\""}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(ids(&json_body(&body)), vec!["1"], "{body}");
}

/// KEYWORD 大小写对齐（P1 hardening）：共享语法（FT.SEARCH 与 ES
/// `query_string`）对 KEYWORD 字段的 scoped 子句改按 AS-TYPED 原始
/// 字节精确查 posting——与索引侧 "exact bytes, NO tokenize/lowercase"
/// 及 ES `term`/`match` 的 keyword 路径一致。此前查询侧一律 tokenize
/// 小写化，混大小写 keyword 值经两个前端都静默零命中。
#[tokio::test]
async fn keyword_exact_case_both_fronts() {
    let (_node, es) = spawn_es_ready("kwcase").await;
    let resp_addr = _node.resp.clone();

    let r = resp_text(
        &resp_addr,
        &[
            b"ft.create",
            b"kidx",
            b"SCHEMA",
            b"kw",
            b"KEYWORD",
            b"body",
            b"TEXT",
        ],
    )
    .await;
    assert!(r.starts_with("+OK"), "ft.create: {r}");
    for (id, doc) in [
        ("k1", r#"{"kw":"RedisLabs","body":"case sensitivity doc"}"#),
        ("k2", r#"{"kw":"Redis Labs","body":"spaced keyword value"}"#),
    ] {
        let r = resp_text(
            &resp_addr,
            &[b"ft.add", b"kidx", id.as_bytes(), doc.as_bytes()],
        )
        .await;
        assert!(r.starts_with(":1"), "ft.add {id}: {r}");
    }

    // RESP：混大小写 keyword 按原始字节命中；错误大小写零命中（精确语义）
    let r = resp_call(&resp_addr, &[b"ft.search", b"kidx", b"@kw:RedisLabs"]).await;
    assert!(r.contains("k1"), "exact case hits: {r:?}");
    let r = resp_call(&resp_addr, &[b"ft.search", b"kidx", b"@kw:redislabs"]).await;
    assert!(r.contains(":0\r\n"), "wrong case misses: {r:?}");
    // 带空格的 keyword 值：引号短语体按整值精确命中
    let r = resp_call(&resp_addr, &[b"ft.search", b"kidx", b"@kw:\"Redis Labs\""]).await;
    assert!(r.contains("k2"), "quoted whole-value match: {r:?}");

    // ES query_string 走同一语法；term 的 keyword 精确路径保持不变
    let (status, out) = http(
        &es,
        "POST",
        "/kidx/_search",
        Some(r#"{"query":{"query_string":{"query":"kw:RedisLabs"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{out}");
    let v = json_body(&out);
    assert_eq!(v["hits"]["total"]["value"], 1, "{v}");
    assert_eq!(v["hits"]["hits"][0]["_id"], "k1");

    let (status, out) = http(
        &es,
        "POST",
        "/kidx/_search",
        Some(r#"{"query":{"query_string":{"query":"kw:redislabs"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        json_body(&out)["hits"]["total"]["value"],
        0,
        "wrong case: {out}"
    );

    let (status, out) = http(
        &es,
        "POST",
        "/kidx/_search",
        Some(r#"{"query":{"term":{"kw":"RedisLabs"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        json_body(&out)["hits"]["total"]["value"],
        1,
        "term exact: {out}"
    );

    // TEXT 路径不受影响（tokenize 小写化照旧）
    let (status, out) = http(
        &es,
        "POST",
        "/kidx/_search",
        Some(r#"{"query":{"match":{"body":"case"}}}"#),
    )
    .await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        json_body(&out)["hits"]["total"]["value"],
        1,
        "text path: {out}"
    );
}
