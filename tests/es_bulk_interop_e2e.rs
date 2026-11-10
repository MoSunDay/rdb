//! Process-level e2e for the Wukong-FT WP6 interop matrix: ES `_bulk`
//! NDJSON writes vs RESP FT.SEARCH phrase/prefix, mixed-front writes
//! (FT.ADD / ES PUT / `_bulk`) onto ONE index with cross-front docid
//! overwrites, the WP4 `@price:[..]` + SORTBY / query_string numeric
//! bridge, and the EXPIRE-driven whole-family TTL deletion seen from
//! both fronts. Complements `resp_es_interop_one_kernel` and
//! `es_stopwords_across_fronts` in es_search_e2e.rs (no overlap);
//! this file exists because adding the matrix there would push it
//! past the 800-line file budget (helpers copied per the repo's
//! per-binary test-mount convention).

mod common;
mod es_common;

use es_common::{http, json_body, spawn_es_ready};

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{cmd_one_shot, TOKEN};

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
/// pipelined before the command): FT.SEARCH answers with an array whose
/// total and hit ids live deeper in the frame. Reads until a 300ms idle
/// gap. (Same reader as es_search_e2e.rs, copied for this binary.)
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

/// FT.CREATE，断言 +OK（schema 作为尾部参数表传入）。
async fn ft_create_ok(resp: &str, idx: &str, schema: &[&[u8]]) {
    let mut args = vec![b"ft.create".as_slice(), idx.as_bytes()];
    args.extend_from_slice(schema);
    let r = resp_text(resp, &args).await;
    assert!(r.starts_with("+OK"), "ft.create {idx}: {r}");
}

/// FT.ADD，断言 :1（本实现新增与覆盖同形，恒为 :1）。
async fn ft_add_ok(resp: &str, idx: &str, id: &str, doc: &str) {
    let mut args = vec![b"ft.add".as_slice(), idx.as_bytes()];
    args.extend([id.as_bytes(), doc.as_bytes()]);
    let r = resp_text(resp, &args).await;
    assert!(r.starts_with(":1"), "ft.add {id}: {r}");
}

/// 索引级 `POST /{idx}/_bulk`：断言 200 且 errors:false。
async fn bulk_ok(es: &str, idx: &str, ndjson: &str) {
    let (status, body) = http(es, "POST", &format!("/{idx}/_bulk"), Some(ndjson)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(json_body(&body)["errors"], false, "{body}");
}

/// ES PUT 单文档：created 201 / updated 200 均放行，返回响应 JSON。
async fn es_put(es: &str, idx: &str, id: &str, doc: &str) -> serde_json::Value {
    let (status, body) = http(es, "PUT", &format!("/{idx}/_doc/{id}"), Some(doc)).await;
    assert!(status == 200 || status == 201, "put {id}: {status} {body}");
    json_body(&body)
}

/// ES `_search`（跨索引版本，断言 200）。
async fn es_query(es: &str, idx: &str, body: &str) -> serde_json::Value {
    let (status, out) = http(es, "POST", &format!("/{idx}/_search"), Some(body)).await;
    assert_eq!(status, 200, "{idx} {body} -> {out}");
    json_body(&out)
}

/// bidx/midx/tidx 共用的两字段 schema（含 SCHEMA 关键字）。
const SCHEMA_BP: &[&[u8]] = &[b"SCHEMA", b"body", b"TEXT", b"price", b"NUMERIC"];

/// WP6 互操作矩阵（与 es_search_e2e::resp_es_interop_one_kernel 互补）：
/// ① ES `_bulk`（索引级路径，action 行只带 `_id`）写 3 篇——两条含短语
/// "deep blue sea"、一条含前缀词 quantum——RESP scoped 短语/裸前缀命中，
/// 反向 ES match_phrase 也命中（`_bulk` 与 PUT 走同一条 put_doc 路径）；
/// ② FT.ADD / ES PUT / `_bulk` / FT.ADD 混写同一索引，m1 由 ES 覆盖、
/// m2 由 RESP 覆盖：覆盖不重复计数，双方都能搜到对方最后一次写入；
/// ③ WP4 跨前端：`@price:[..]` + SORTBY 升序 ↔ query_string 数值区间；
/// ④ TTL 家族整删（机制照抄 search_e2e::ft_ttl_expires_whole_family：
/// RESP `EXPIRE <索引名> 1` + sleep，读到过期即整删索引 meta + postings
/// + 文档）后，两前端对同一物理删除的视图一致。
#[tokio::test]
async fn es_bulk_and_ttl_interop() {
    let (_node, es) = spawn_es_ready("interop2").await;
    let resp_addr = _node.resp.clone();
    // 按索引柯里化的小动作，调用点一行一个。
    let badd = async |i: &str, d: &str| ft_add_ok(&resp_addr, "bidx", i, d).await;
    let madd = async |i: &str, d: &str| ft_add_ok(&resp_addr, "midx", i, d).await;
    let tadd = async |i: &str, d: &str| ft_add_ok(&resp_addr, "tidx", i, d).await;
    let mput = async |i: &str, d: &str| es_put(&es, "midx", i, d).await;
    let tput = async |i: &str, d: &str| es_put(&es, "tidx", i, d).await;
    let bq = async |q: &str| es_query(&es, "bidx", q).await;
    let mq = async |q: &str| es_query(&es, "midx", q).await;

    // ① `_bulk` 写入 ↔ RESP 短语/前缀。
    ft_create_ok(&resp_addr, "bidx", SCHEMA_BP).await;
    let nd = r#"{"index":{"_id":"b1"}}
{"body":"sail the deep blue sea today","price":30}
{"index":{"_id":"b2"}}
{"body":"another deep blue sea story","price":10}
{"index":{"_id":"b3"}}
{"body":"quantum computing explained","price":20}
"#;
    bulk_ok(&es, "bidx", nd).await;
    let r = resp_call(&resp_addr, &[b"ft.search", b"bidx", br#"@body:"blue sea""#]).await;
    assert!(r.contains(":2\r\n"), "phrase total: {r:?}");
    assert!(r.contains("b1") && r.contains("b2"), "phrase ids: {r:?}");
    // 裸前缀（无 @scope：叶子 field 为 None，扫全部 TEXT 字段）。
    let r = resp_call(&resp_addr, &[b"ft.search", b"bidx", b"quant*"]).await;
    assert!(r.contains(":1\r\n") && r.contains("b3"), "{r:?}");
    // 反向：ES match_phrase 命中 `_bulk` 写入的相邻位置。
    let v = bq(r#"{"query":{"match_phrase":{"body":"blue sea"}}}"#).await;
    let mut got = ids(&v);
    got.sort();
    assert_eq!(got, vec!["b1", "b2"], "{v}");

    // ② 混写一致性：四种写路径交替 + 双向覆盖同一 docid。
    ft_create_ok(&resp_addr, "midx", SCHEMA_BP).await;
    madd("m1", r#"{"body":"alpha resp one","price":1}"#).await;
    mput("m2", r#"{"body":"beta es one","price":2}"#).await;
    let nd = r#"{"index":{"_id":"m3"}}
{"body":"gamma bulk one","price":3}
"#;
    bulk_ok(&es, "midx", nd).await;
    madd("m4", r#"{"body":"delta resp two","price":4}"#).await;
    let v = mput("m1", r#"{"body":"alpha es v2","price":11}"#).await;
    assert_eq!(v["result"], "updated", "es overwrite: {v}");
    madd("m2", r#"{"body":"beta resp v2","price":12}"#).await;
    // 两前端 total 相等（覆盖不重复计数）。
    let r = resp_call(&resp_addr, &[b"ft.search", b"midx", b"*"]).await;
    assert!(r.contains(":4\r\n"), "resp total: {r:?}");
    let v = mq(r#"{"query":{"match_all":{}}}"#).await;
    assert_eq!(v["hits"]["total"]["value"], 4, "es total: {v}");
    // RESP 看到 ES 的最后一次写入（内容片段），ES 看到 RESP 的（_source）。
    let r = resp_call(&resp_addr, &[b"ft.search", b"midx", b"@body:v2"]).await;
    assert!(r.contains("m1") && r.contains("alpha es v2"), "{r:?}");
    let v = mq(r#"{"query":{"match_phrase":{"body":"beta resp v2"}}}"#).await;
    assert_eq!(ids(&v), vec!["m2"], "{v}");
    let src = &v["hits"]["hits"][0]["_source"];
    assert_eq!(src["body"], "beta resp v2", "{v}");

    // ③ WP4 顺手矩阵：b1..b3 已带 price 30/10/20，FT.ADD 补 40。
    badd("b4", r#"{"body":"resp numeric doc","price":40}"#).await;
    let q = vec![
        b"ft.search".as_slice(),
        b"bidx".as_slice(),
        b"@price:[10 25]",
        b"SORTBY",
        b"price",
        b"ASC",
    ];
    let r = resp_call(&resp_addr, &q).await;
    assert!(r.contains(":2\r\n"), "range total: {r:?}");
    assert!(r.find("b2") < r.find("b3"), "sortby asc: {r:?}");
    let qs = r#"{"query":{"query_string":{"query":"price:[35 45] AND body:resp"}}}"#;
    assert_eq!(ids(&bq(qs).await), vec!["b4"], "qs range hits ft.add");

    // ④ TTL：混写两篇后 EXPIRE 索引键 1s，两前端视图一致。
    ft_create_ok(&resp_addr, "tidx", SCHEMA_BP).await;
    tadd("t1", r#"{"body":"ttl resp doc","price":1}"#).await;
    tput("t2", r#"{"body":"ttl es doc","price":2}"#).await;
    let r = resp_text(&resp_addr, &[b"expire", b"tidx", b"1"]).await;
    assert!(r.starts_with(":1"), "expire index key: {r}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let r = resp_call(&resp_addr, &[b"ft.search", b"tidx", b"*"]).await;
    assert!(r.contains("unknown index"), "resp after ttl: {r:?}");
    let (status, body) = http(&es, "GET", "/tidx", None).await;
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("index_not_found_exception"), "{body}");
    let (status, body) = http(&es, "GET", "/tidx/_doc/t2", None).await;
    assert_eq!(status, 404, "doc purged with family: {body}");
    assert_eq!(json_body(&body)["found"], false, "{body}");
}
