//! Batch 11 scenario #121 -- an order-history desk pages a long order
//! book through ONE prepared statement: `?`-bound LIMIT/OFFSET give
//! page 1 and page 2 (disjoint, ordered, no gaps), `LIMIT 0` answers
//! an empty page, a negative LIMIT and the two-placeholder `LIMIT ?, ?`
//! spelling are parse errors (1064), the statement re-executes 20
//! times inside the budget, and after `DEALLOCATE PREPARE` (the binary
//! protocol's COM_STMT_CLOSE) the stale handle never answers the page
//! again (the protocol layer rejects the unknown statement id). The ES face serves the same page-2 window
//! (`from`/`size`) as the cross-check. One REAL node, MySQL + ES.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, run, server_error, ER_PARSE_ERROR};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, FrontSpec};

const PASS: &str = "e2e-sql-pass";
const SQL: &str = "SELECT id FROM orders WHERE id > ? ORDER BY id LIMIT ? OFFSET ?";

/// One wire cell as i64 (the text and binary paths spell ints
/// differently: Bytes vs Int).
fn cell_i64(v: &MVal) -> i64 {
    match v {
        MVal::Int(i) => *i,
        MVal::Bytes(b) => std::str::from_utf8(b)
            .unwrap()
            .parse()
            .unwrap_or_else(|e| panic!("int cell {:?}: {e}", v)),
        other => panic!("expected int cell, got {other:?}"),
    }
}

/// Execute the prepared page statement and collect the id column.
async fn page(
    c: &mut mysql_async::Conn,
    stmt: &mysql_async::Statement,
    params: (i64, i64, i64),
) -> Vec<i64> {
    let rs: Vec<mysql_async::Row> = c
        .exec(stmt, params)
        .await
        .unwrap_or_else(|e| panic!("exec page {params:?}: {e}"));
    rs.iter()
        .map(|r| cell_i64(&r.get::<MVal, _>(0).unwrap_or(MVal::NULL)))
        .collect()
}

/// A second connection whose statement cache holds ONE entry, so
/// preparing a second statement evicts (COM_STMT_CLOSE) the first --
/// the binary-protocol `DEALLOCATE PREPARE`.
async fn connect_cache1(node: &common::ProcNode) -> mysql_async::Conn {
    let port = node
        .mysql
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .expect("mysql port");
    let opts = mysql_async::OptsBuilder::default()
        .ip_or_hostname("127.0.0.1")
        .tcp_port(port)
        .user(Some("root"))
        .pass(Some(PASS))
        .stmt_cache_size(1);
    mysql_async::Conn::new(opts)
        .await
        .unwrap_or_else(|e| panic!("mysql connect cache1: {e}"))
}

/// ES `_search` ids of the `orders` index, sorted by `id` asc.
async fn es_page(es: &str, from: usize, size: usize) -> Vec<String> {
    let (_status, body) = http(
        es,
        "POST",
        "/orders/_search",
        Some(&format!(
            r#"{{"sort":[{{"id":{{"order":"asc"}}}}],"from":{from},"size":{size},"query":{{"match_all":{{}}}}}}"#
        )),
    )
    .await;
    let v = json_of(&body);
    v["hits"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("no hits array: {body}"))
        .iter()
        .map(|h| h["_id"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn order_history_prepared_pages_and_es_window() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, _a) = front_world("prep121", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;

    // a) 30 orders, ids 1..30.
    ddl(
        &mut c,
        "CREATE TABLE orders (id BIGINT PRIMARY KEY, item VARCHAR(32), amount DECIMAL(10,2))",
    )
    .await;
    for i in 1..=30 {
        run(
            &mut c,
            &format!("INSERT INTO orders (id, item, amount) VALUES ({i}, 'sku{i}', {i}.25)"),
        )
        .await;
    }

    // b) ONE prepared statement pages the book: LIMIT/OFFSET ride the
    //    bound parameters, so the same handle serves every window.
    let stmt = c.prep(SQL).await.expect("prepare page statement");
    let t = scenario_common::perf::now();
    let page1 = page(&mut c, &stmt, (0, 10, 0)).await;
    let page2 = page(&mut c, &stmt, (0, 10, 10)).await;
    scenario_common::perf::within("prep121/2-first-pages", t, Duration::from_secs(2));
    let want1: Vec<i64> = (1..=10).collect();
    let want2: Vec<i64> = (11..=20).collect();
    assert_eq!(page1, want1, "page 1");
    assert_eq!(page2, want2, "page 2");
    // Disjoint, ordered, and contiguous: page 2 resumes exactly where
    // page 1 stopped (no gap, no duplicate).
    assert!(
        page1.iter().all(|id| !page2.contains(id)),
        "pages overlap: {page1:?} vs {page2:?}"
    );
    let mut both = page1.clone();
    both.extend(page2.iter().copied());
    assert!(both.windows(2).all(|w| w[0] < w[1]), "not ordered: {both:?}");
    assert_eq!(both, (1..=20).collect::<Vec<i64>>(), "gap in the paging");

    // c) LIMIT 0 is a legal empty window (the bind coerces to u64 0).
    assert!(page(&mut c, &stmt, (0, 0, 0)).await.is_empty(), "LIMIT 0");

    // d) A negative LIMIT is a parse error, and so is the ambiguous
    //    two-placeholder comma form (the binder walks limit-then-offset,
    //    so `LIMIT ?, ?` would swap the user's operands).
    let e = server_error(
        &mut c,
        "SELECT id FROM orders WHERE id > 0 ORDER BY id LIMIT -1",
    )
    .await;
    assert_eq!(e.code, ER_PARSE_ERROR, "{e:?}");
    let e = c
        .prep("SELECT id FROM orders WHERE id > ? ORDER BY id LIMIT ?, ?")
        .await
        .expect_err("two placeholders in LIMIT ?, ?");
    match e {
        mysql_async::Error::Server(se) => {
            assert_eq!(se.code, ER_PARSE_ERROR, "{se:?}");
            assert!(se.message.contains("LIMIT"), "{se:?}");
        }
        other => panic!("expected a server error, got {other}"),
    }

    // e) Re-execution cost: 20 more executions of the SAME handle
    //    (no re-parse, no re-prepare) alternate the two windows.
    let t = scenario_common::perf::now();
    for i in 0..20 {
        let (want, params) = if i % 2 == 0 {
            (&want1, (0, 10, 0))
        } else {
            (&want2, (0, 10, 10))
        };
        assert_eq!(page(&mut c, &stmt, params).await, *want, "re-exec {i}");
    }
    scenario_common::perf::within("prep121/20-reexec", t, Duration::from_secs(2));

    // f) DEALLOCATE PREPARE is the binary protocol's COM_STMT_CLOSE: the
    //    handle leaves the session, so executing it again does NOT
    //    answer MySQL's 1243 ERR packet. opensrv's command loop looks
    //    the id up in ITS statement map before the shim's on_execute
    //    (whose "unknown prepared statement" -> 1243 branch in
    //    `src/sql/front/shim.rs` is unreachable for a closed id): the
    //    miss is an InvalidData io::Error that ends the connection, so
    //    the driver surfaces Io. A Server(1243) reply would mean the
    //    protocol layer let the shim see the stale id -- the opposite
    //    of the recorded deviation -- and ANY row set would be the stale
    //    handle answering its page again; both fail here.
    let mut d = connect_cache1(&front.node).await;
    let stale = d.prep(SQL).await.expect("prepare on the dealloc conn");
    let _evict = d.prep("SELECT 1").await.expect("prepare evicts + closes");
    d.query_drop("SELECT 3").await.expect("session alive after the close");
    match d.exec::<mysql_async::Row, _, _>(&stale, (0, 10, 0)).await {
        Err(e) => assert!(
            matches!(e, mysql_async::Error::Io(_)),
            "the stale handle reached the shim (1243) or another server error: {e:?}"
        ),
        Ok(rows) => panic!(
            "a deallocated handle answered {} rows: {rows:?}",
            rows.len()
        ),
    }

    // A fresh session prepares and executes the same shape again.
    let mut d2 = connect_cache1(&front.node).await;
    let again = d2.prep(SQL).await.expect("re-prepare on a fresh session");
    let ids = page(&mut d2, &again, (0, 10, 10)).await;
    assert_eq!(ids, want2, "fresh handle pages the same window");

    // g) ES cross-check: the same 30 orders as documents, paged with
    //    from/size in the id order -- page 2 is the id set the SQL
    //    statement answered.
    let (status, body) = http(
        &es,
        "PUT",
        "/orders",
        Some(r#"{"mappings":{"properties":{"id":{"type":"long"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create orders index: {body}");
    for i in 1..=30 {
        let (status, body) = http(
            &es,
            "PUT",
            &format!("/orders/_doc/o{i}"),
            Some(&format!(r#"{{"id":{i}}}"#)),
        )
        .await;
        assert_eq!(status, 201, "index o{i}: {body}");
    }
    let ids: Vec<String> = es_page(&es, 10, 10)
        .await
        .iter()
        .map(|id| id.trim_start_matches('o').to_string())
        .collect();
    assert_eq!(ids, want2.iter().map(|i| i.to_string()).collect::<Vec<_>>());
    let total: Vec<String> = es_page(&es, 0, 30)
        .await
        .iter()
        .map(|id| id.trim_start_matches('o').to_string())
        .collect();
    assert_eq!(total.len(), 30, "full window: {total:?}");
    assert_eq!(total[0], "1", "{total:?}");
    assert_eq!(total[29], "30", "{total:?}");

    scenario_common::perf::scenario("prep121", t0);
}

