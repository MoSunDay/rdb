//! Multi-store scenario #116 -- a product-catalog A/B: the SAME twenty
//! products live as HASHes (`product:<n>`), as documents of one FT
//! index and as rows of a SQL table: every match-all hit's `HGETALL`
//! reads back as its FT document, and every FT.SEARCH answer is
//! cross-checked on two other engines. Pinned: the FT.CREATE surface
//! of this face (no `ON HASH PREFIX 1` and no TAG field type -- both
//! refused; JSON documents ride FT.ADD), match-all + `SORTBY price`
//! (full match set, cheapest first), `RETURN` projection in requested
//! order and `RETURN 0` -> `{}`, `HIGHLIGHT ... TAGS` wrapping the
//! matched title term, `WITHSCORES` scores spelled with 4 decimals,
//! and an ES `term` query plus a SQL `WHERE` agreeing on the red
//! products. One REAL node, MySQL + ES.

use crate::common;
use crate::scenario_common;
use scenario_common::{bulk_at, front_world, http, int_of, json_of, r, FrontSpec};
use std::time::Duration;

/// The FT index of this scenario (its own key space, no hash tag).
const IDX: &str = "ft116idx";
const COLORS: [&str; 3] = ["red", "blue", "green"];
const NOUNS: [&str; 5] = [
    "running shoes",
    "denim jacket",
    "garden hose",
    "desk lamp",
    "coffee mug",
];

/// Product `i` of the 20-line catalog: `(title, price, color)`.
fn product(i: usize) -> (String, i64, &'static str) {
    let color = COLORS[i % COLORS.len()];
    (
        format!("{color} {}", NOUNS[i % NOUNS.len()]),
        5 + i as i64,
        color,
    )
}

/// Key of product `i`: the HASH key, the FT doc id AND the ES doc id.
fn key(i: usize) -> String {
    format!("product:{}", i + 1)
}

/// Indexed JSON of product `i` (the HASH fields, verbatim).
fn doc_json(i: usize) -> String {
    let (title, price, color) = product(i);
    format!(r#"{{"title":"{title}","price":{price},"color":"{color}"}}"#)
}

/// How many catalog lines carry `color`.
fn color_count(color: &str) -> usize {
    (0..20).filter(|i| product(*i).2 == color).count()
}

/// Bytes of a `&str` command line.
fn argv<'a>(args: &'a [&'a str]) -> Vec<&'a [u8]> {
    args.iter().map(|t| t.as_bytes()).collect()
}

/// Raw text reply of one RESP command.
async fn cmd(addr: &str, args: &[&str]) -> String {
    common::lite::text(&r(addr, &argv(args)).await)
}

/// Integer reply of one RESP command.
async fn cmd_int(addr: &str, args: &[&str]) -> i64 {
    int_of(&r(addr, &argv(args)).await)
}

/// Full-drain reply text with a SHORT quiet gap: the fixture's `rf`
/// parks 250ms per call, which alone would blow the 3s gate below.
async fn drain(addr: &str, args: &[&str]) -> String {
    common::lite::text(
        &common::lite::cmd_full_reply_wait(addr, common::TOKEN, &argv(args), 40, 10_000).await,
    )
}

/// `FT.SEARCH <IDX> <args...>` reply text.
async fn raw(addr: &str, args: &[&str]) -> String {
    let mut full = vec!["FT.SEARCH", IDX];
    full.extend_from_slice(args);
    drain(addr, &full).await
}

/// The integer total of an FT.SEARCH reply: a FLAT array whose first
/// element is the `:total` header (the array length is NOT the total).
fn total_of(reply: &str) -> usize {
    reply
        .lines()
        .find_map(|l| l.strip_prefix(':').and_then(|t| t.parse().ok()))
        .unwrap_or_else(|| panic!("no total in {reply}"))
}

/// Doc content cells of a reply without WITHSCORES (docid, content).
fn contents(reply: &str) -> Vec<String> {
    (0..total_of(reply))
        .map(|h| bulk_at(reply.as_bytes(), h * 2 + 1))
        .collect()
}

/// Score cells of a WITHSCORES reply (docid, score, content).
fn scores(reply: &str) -> Vec<String> {
    (0..total_of(reply))
        .map(|h| bulk_at(reply.as_bytes(), h * 3 + 1))
        .collect()
}

/// `(total, doc content cells)` of one FT.SEARCH.
async fn hit(addr: &str, args: &[&str]) -> (usize, Vec<String>) {
    let reply = raw(addr, args).await;
    (total_of(&reply), contents(&reply))
}

#[tokio::test]
async fn product_catalog_three_engines_agree() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        es: true,
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("ft116", &spec).await;
    let es = front.node.es.clone();

    // a) The RediSearch-shaped DDL of the brief is NOT this face's
    //    syntax: `ON HASH PREFIX 1` is an unknown option and `TAG` an
    //    unknown field type. Both are pinned; the real surface is
    //    `FT.CREATE <idx> SCHEMA <f> TEXT|KEYWORD|NUMERIC ...` over
    //    FT.ADD JSON documents.
    let e = cmd(
        &a,
        &[
            "FT.CREATE",
            IDX,
            "ON",
            "HASH",
            "PREFIX",
            "1",
            "product:",
            "SCHEMA",
            "price",
            "NUMERIC",
        ],
    )
    .await;
    assert!(e.contains("unknown option"), "ON HASH PREFIX 1: {e}");
    let e = cmd(
        &a,
        &["FT.CREATE", IDX, "SCHEMA", "title", "TEXT", "color", "TAG"],
    )
    .await;
    assert!(e.contains("unknown field type"), "TAG error: {e}");
    let ok = cmd(
        &a,
        &[
            "FT.CREATE",
            IDX,
            "SCHEMA",
            "price",
            "NUMERIC",
            "title",
            "TEXT",
            "color",
            "KEYWORD",
        ],
    )
    .await;
    assert!(ok.starts_with("+OK"), "create index: {ok}");

    // b) The hot block: 20 HASH writes + 20 FT.ADDs + 20 searches
    //    (4 rounds over the 4 shapes asserted below).
    let t = scenario_common::perf::now();
    for i in 0..20 {
        let (title, price, color) = product(i);
        let k = key(i);
        let hset = cmd_int(
            &a,
            &[
                "HSET",
                &k,
                "title",
                &title,
                "price",
                &price.to_string(),
                "color",
                color,
            ],
        )
        .await;
        assert_eq!(hset, 3, "hset {k}");
        assert_eq!(
            cmd_int(&a, &["FT.ADD", IDX, &k, &doc_json(i)]).await,
            1,
            "ft.add {k}"
        );
    }
    for _ in 0..5 {
        let (total, docs) = hit(&a, &["*", "SORTBY", "price", "LIMIT", "0", "20"]).await;
        assert_eq!((total, docs.len()), (20, 20), "match-all page");
        let red = color_count("red");
        assert_eq!(
            hit(&a, &["red", "RETURN", "2", "title", "price"]).await.0,
            red
        );
        assert_eq!(
            hit(
                &a,
                &[
                    "red",
                    "HIGHLIGHT",
                    "FIELDS",
                    "1",
                    "title",
                    "TAGS",
                    "<b>",
                    "</b>"
                ]
            )
            .await
            .0,
            red
        );
        assert_eq!(
            scores(&raw(&a, &["red", "WITHSCORES", "LIMIT", "0", "20"]).await).len(),
            red
        );
    }
    scenario_common::perf::within("ft116/20-add+20-search", t, Duration::from_secs(3));

    // c) Match-all sorted by price: 20 hits, the cheapest (price 5)
    //    leads, and every page slot carries its price in order -- and
    //    the HASH face is the same catalog: each hit's doc id is a HASH
    //    key whose HGETALL reads back field-for-field as the FT document
    //    (hash values are bulk strings, so price compares spelled out).
    let page = raw(&a, &["*", "SORTBY", "price", "LIMIT", "0", "20"]).await;
    assert_eq!(bulk_at(page.as_bytes(), 0), key(0), "first hit id: {page}");
    let docs = contents(&page);
    assert_eq!(docs.len(), 20, "total: {page}");
    assert_eq!(json_of(&docs[0])["price"], 5, "first hit price: {docs:?}");
    for (i, content) in docs.iter().enumerate() {
        let (id, doc) = (bulk_at(page.as_bytes(), i * 2), json_of(content));
        assert_eq!(doc["price"], 5 + i as i64, "price order at {i}: {docs:?}");
        let all = drain(&a, &["HGETALL", &id]).await;
        let spell = |v: &serde_json::Value| v.as_str().map_or_else(|| v.to_string(), String::from);
        let want: serde_json::Map<String, serde_json::Value> = doc
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), spell(v).into()))
            .collect();
        assert!(all.starts_with(&format!("*{}", 2 * want.len())), "HGETALL {id}: {all}");
        let cell = |n: usize| bulk_at(all.as_bytes(), n);
        let got: serde_json::Map<String, serde_json::Value> = (0..want.len())
            .map(|f| (cell(f * 2), cell(f * 2 + 1).into()))
            .collect();
        assert_eq!(got, want, "HGETALL {id} vs its FT document: {all}");
    }

    // d) RETURN projection: the two named fields in the REQUESTED
    //    order (title first, then price -- not the stored JSON order of
    //    the document), and RETURN 0 -> `{}` for every hit.
    let (total, projected) = hit(&a, &["red", "RETURN", "2", "title", "price"]).await;
    assert_eq!(total, color_count("red"), "{projected:?}");
    assert_eq!(
        projected[0], r#"{"title":"red running shoes","price":5}"#,
        "{projected:?}"
    );
    let (_, empty) = hit(&a, &["red", "RETURN", "0"]).await;
    assert!(
        !empty.is_empty() && empty.iter().all(|c| c == "{}"),
        "RETURN 0: {empty:?}"
    );

    // e) HIGHLIGHT FIELDS 1 title TAGS <b> </b>: every hit's title
    //    carries the wrapped term, no raw title survives, and FIELDS
    //    keeps the KEYWORD color unwrapped.
    let (_, hl) = hit(
        &a,
        &[
            "red",
            "HIGHLIGHT",
            "FIELDS",
            "1",
            "title",
            "TAGS",
            "<b>",
            "</b>",
        ],
    )
    .await;
    assert_eq!(hl.len(), color_count("red"), "{hl:?}");
    for content in &hl {
        assert!(
            content.contains(r#""title":"<b>red</b> "#),
            "wrapped: {content}"
        );
        assert!(
            !content.contains(r#""title":"red "#),
            "raw title leaked: {content}"
        );
        assert!(
            content.contains(r#""color":"red""#),
            "keyword kept: {content}"
        );
    }

    // f) WITHSCORES: 4-decimal score cells, descending; the match-all
    //    score is the constant 1.0000.
    let cells = scores(&raw(&a, &["red", "WITHSCORES", "LIMIT", "0", "20"]).await);
    assert_eq!(cells.len(), color_count("red"), "{cells:?}");
    let mut prev = f64::MAX;
    for cell in &cells {
        assert_eq!(
            cell.split('.').nth(1).unwrap_or("").len(),
            4,
            "score decimals: {cell}"
        );
        let v: f64 = cell.parse().unwrap();
        assert!(v > 0.0 && v <= prev, "score order: {cells:?}");
        prev = v;
    }
    let constant = raw(&a, &["*", "WITHSCORES", "LIMIT", "0", "1"]).await;
    assert_eq!(
        bulk_at(constant.as_bytes(), 1),
        "1.0000",
        "match-all score: {constant}"
    );

    // g) ES cross-check over the same corpus: a term query on the red
    //    products agrees with the FT.SEARCH count and price vector.
    let maps = r#"{"mappings":{"properties":{"title":{"type":"text"},"price":{"type":"long"},"color":{"type":"keyword"}}}}"#;
    let (status, body) = http(&es, "PUT", "/products", Some(maps)).await;
    assert_eq!(status, 200, "create products: {body}");
    let mut ndjson = String::new();
    for i in 0..20 {
        let action = format!(
            "{{\"index\":{{\"_index\":\"products\",\"_id\":\"{}\"}}}}",
            key(i)
        );
        ndjson.push_str(&format!("{action}\n{}\n", doc_json(i)));
    }
    let (status, body) = http(&es, "POST", "/_bulk", Some(&ndjson)).await;
    assert_eq!(status, 200, "bulk products: {body}");
    assert_eq!(json_of(&body)["errors"], false, "{body}");
    let term = r#"{"query":{"term":{"color":"red"}},"size":20}"#;
    let (status, body) = http(&es, "POST", "/products/_search", Some(term)).await;
    assert_eq!(status, 200, "es term: {body}");
    let v = json_of(&body);
    assert_eq!(
        v["hits"]["total"]["value"],
        color_count("red") as u64,
        "ES term count: {body}"
    );
    let mut prices: Vec<i64> = v["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_source"]["price"].as_i64().unwrap())
        .collect();
    prices.sort_unstable();
    let (_, red) = hit(&a, &["@color:red", "SORTBY", "price", "LIMIT", "0", "20"]).await;
    let ft_prices: Vec<i64> = red
        .iter()
        .map(|c| json_of(c)["price"].as_i64().unwrap())
        .collect();
    assert_eq!(ft_prices, prices, "FT vs ES red prices: {red:?}");

    // h) The third leg: the same catalog in SQL answers the identical
    //    price vector, so all three engines agree on the red products.
    let mut c = common::mysql::connect_root(&front.node).await;
    let ddl = "CREATE TABLE catalog (id BIGINT PRIMARY KEY, title VARCHAR(32) NOT NULL, \
               price BIGINT NOT NULL, color VARCHAR(8) NOT NULL)";
    common::mysql::ddl(&mut c, ddl).await;
    for i in 0..20 {
        let (title, price, color) = product(i);
        let sql = format!(
            "INSERT INTO catalog VALUES ({}, '{title}', {price}, '{color}')",
            i + 1
        );
        common::mysql::run(&mut c, &sql).await;
    }
    let cells = common::mysql::col(
        &mut c,
        "SELECT price FROM catalog WHERE color = 'red' ORDER BY price",
    )
    .await;
    let sql_prices: Vec<i64> = cells
        .iter()
        .map(|v| match v {
            mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
            other => panic!("non-bytes price {other:?}"),
        })
        .collect();
    assert_eq!(sql_prices, prices, "SQL vs ES red prices");
    scenario_common::perf::scenario("ft116", t0);
}
