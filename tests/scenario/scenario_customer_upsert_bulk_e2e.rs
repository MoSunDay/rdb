//! Multi-store scenario #142 -- a CRM syncs customers into ES with
//! `_bulk` (index / update / delete actions, scoped to the index and
//! unscoped), refuses overwrites with `?op_type=create` (409), narrows
//! a bool `should` query with `minimum_should_match` (count and percent
//! forms, and as a gate beside `must`); RESP keeps a hash per live
//! customer and MySQL the billing rows, and all three agree on tier /
//! due / counts. SURFACES: ES + RESP + MySQL. One REAL node, ES + MySQL.
//!
//! Deviation (src/es/bulk.rs:137-147): a `_bulk` `update` action is a
//! per-item 400 `action_request_validation_exception` (errors:true) --
//! the CRM's refresh is done with an `index` action on the same id
//! (200 updated), asserted as such.
//! Deviation (src/es/router.rs:59-66 + bulk.rs:104-109): `_bulk` never
//! reads `?op_type=create` nor a per-action `op_type` -- only the
//! `create` ACTION is create-only; the query form is asserted as ignored.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, rb, rf, r, int_of, FrontSpec};
use serde_json::Value as Json;

const INDEX: &str = "crm_upsert142";
const MAPPINGS: &str = r#"{"mappings":{"properties":{"name":{"type":"keyword"},
    "tier":{"type":"keyword"},"region":{"type":"keyword"},"vip":{"type":"keyword"},
    "due":{"type":"integer"}}}}"#;

/// Wire cell as text (never unwraps a NULL).
fn txt(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::NULL => "<NULL>".to_string(),
        other => format!("{other:?}"),
    }
}

/// One customer document as JSON text (`vip` is a keyword yes/no).
fn customer(name: &str, tier: &str, region: &str, vip: bool, due: i64) -> String {
    format!(
        r#"{{"name":"{name}","tier":"{tier}","region":"{region}","vip":"{}","due":{due}}}"#,
        if vip { "true" } else { "false" }
    )
}

/// ES call -> (status, parsed JSON body).
async fn es(es_addr: &str, method: &str, path: &str, body: Option<&str>) -> (u16, Json) {
    let (status, out) = http(es_addr, method, path, body).await;
    (status, json_of(&out))
}

/// `hits.total.value` + sorted `_id`s of a POST `_search`.
async fn search(es_addr: &str, body: &str) -> (i64, Vec<String>) {
    let (status, v) = es(es_addr, "POST", &format!("/{INDEX}/_search?size=100"), Some(body)).await;
    assert_eq!(status, 200, "{body}: {v}");
    let mut ids: Vec<String> = v["hits"]["hits"]
        .as_array()
        .map(|a| a.iter().map(|h| h["_id"].as_str().unwrap_or_default().to_string()).collect())
        .unwrap_or_default();
    ids.sort();
    (
        v["hits"]["total"]["value"].as_i64().unwrap_or_else(|| panic!("no total in {v}")),
        ids,
    )
}

/// `_count` of a body.
async fn count(es_addr: &str, body: &str) -> i64 {
    let (status, v) = es(es_addr, "POST", &format!("/{INDEX}/_count"), Some(body)).await;
    assert_eq!(status, 200, "{body}: {v}");
    v["count"].as_i64().unwrap_or_else(|| panic!("no count in {v}"))
}

/// bool query: `must` / `should` term clauses as (field, value) pairs
/// plus an optional `minimum_should_match` (a JSON literal).
fn bool_query(must: &[(&str, &str)], should: &[(&str, &str)], msm: Option<&str>) -> String {
    let list = |clauses: &[(&str, &str)]| {
        clauses
            .iter()
            .map(|(f, v)| format!(r#"{{"term":{{"{f}":"{v}"}}}}"#))
            .collect::<Vec<_>>()
            .join(",")
    };
    let mut q = String::from(r#"{"query":{"bool":{"#);
    if !must.is_empty() {
        q.push_str(&format!(r#""must":[{}],"#, list(must)));
    }
    q.push_str(&format!(r#""should":[{}]"#, list(should)));
    if let Some(m) = msm {
        q.push_str(&format!(r#","minimum_should_match":{m}"#));
    }
    q.push_str("}}}");
    q
}

#[tokio::test]
async fn bulk_upsert_create_conflicts_and_msm() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, resp) = front_world("upsert142", &spec).await;
    let (es_addr, mut c) = (front.node.es.clone(), connect_root(&front.node).await);

    // a) The CRM index (no auto-create), then a create-only write: the
    //    first lands (201), the second is a 409 that leaves the stored
    //    document untouched.
    let (status, v) = es(&es_addr, "PUT", &format!("/{INDEX}"), Some(MAPPINGS)).await;
    assert_eq!((status, v["acknowledged"].as_bool()), (200, Some(true)), "{v}");
    let ada = customer("Ada", "gold", "eu", true, 120);
    let path = format!("/{INDEX}/_doc/c1?op_type=create");
    let (status, v) = es(&es_addr, "POST", &path, Some(&ada)).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!((v["result"].as_str(), v["_id"].as_str()), (Some("created"), Some("c1")), "{v}");
    let (status, v) = es(&es_addr, "POST", &path, Some(&customer("Eve", "bronze", "us", false, 5))).await;
    assert_eq!(status, 409, "op_type=create refuses the overwrite: {v}");
    assert_eq!(v["status"], 409, "{v}");
    assert_eq!(v["error"]["type"], "version_conflict_engine_exception", "{v}");
    assert_eq!(v["error"]["root_cause"][0]["type"], "version_conflict_engine_exception", "{v}");
    assert!(
        v["error"]["reason"].as_str().unwrap_or_default().contains("version conflict, document already exists"),
        "{v}"
    );
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_doc/c1"), None).await;
    assert_eq!((status, v["found"].as_bool()), (200, Some(true)), "{v}");
    assert_eq!(v["_source"], json_of(&ada), "the refused write did not land");
    let (status, v) = es(&es_addr, "PUT", &format!("/{INDEX}/_doc/c1"), Some(&customer("Ada", "gold", "eu", true, 150))).await;
    assert_eq!((status, v["result"].as_str()), (200, Some("updated")), "no op_type = replace: {v}");

    // b) Hot block: scoped _bulk (index / update / delete actions, no
    //    _index in the metadata), the refresh via `index`, the
    //    unscoped _bulk, and the minimum_should_match searches.
    let t = scenario_common::perf::now();
    let c2 = customer("Ben", "silver", "us", false, 45);
    let scoped = format!(
        "{{\"index\":{{\"_id\":\"c2\"}}}}\n{c2}\n\
         {{\"update\":{{\"_id\":\"c1\"}}}}\n{{\"doc\":{{\"tier\":\"gold\",\"due\":120}}}}\n\
         {{\"delete\":{{\"_id\":\"ghost\"}}}}\n\
         {{\"index\":{{\"_id\":\"c3\"}}}}\n{}\n\
         {{\"delete\":{{\"_id\":\"c3\"}}}}\n",
        customer("Cyd", "bronze", "us", false, 7)
    );
    let (status, v) = es(&es_addr, "POST", &format!("/{INDEX}/_bulk"), Some(&scoped)).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["errors"], true, "the update action flips the flag: {v}");
    let items = v["items"].as_array().expect("items");
    assert_eq!(items.len(), 5, "{v}");
    let item = |i: usize, action: &str| -> (i64, Option<&str>) {
        let it = &items[i][action];
        assert_eq!(it["_index"], INDEX, "scoped bulk stamps the URL index: {v}");
        (it["status"].as_i64().unwrap_or(-1), it["result"].as_str())
    };
    assert_eq!(item(0, "index"), (201, Some("created")), "{v}");
    assert_eq!(item(1, "update"), (400, None), "update is a per-item error: {v}");
    assert_eq!(items[1]["update"]["error"]["type"], "action_request_validation_exception", "{v}");
    assert_eq!(item(2, "delete"), (404, Some("not_found")), "delete miss: {v}");
    assert_eq!(item(3, "index"), (201, Some("created")), "{v}");
    assert_eq!(item(4, "delete"), (200, Some("deleted")), "{v}");
    let ada_v2 = customer("Ada", "gold", "eu", true, 120);
    let (status, v) = es(&es_addr, "PUT", &format!("/{INDEX}/_doc/c1"), Some(&ada_v2)).await;
    assert_eq!((status, v["result"].as_str()), (200, Some("updated")), "the refresh the CRM wanted: {v}");
    let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_doc/c1"), None).await;
    assert_eq!((status, v["_source"].clone()), (200, json_of(&ada_v2)), "{v}");
    let c4 = customer("Dan", "gold", "us", true, 200);
    let c5 = customer("Eve", "bronze", "eu", false, 10);
    let unscoped = format!(
        "{{\"index\":{{\"_index\":\"{INDEX}\",\"_id\":\"c4\"}}}}\n{c4}\n{{\"index\":{{\"_index\":\"{INDEX}\",\"_id\":\"c5\"}}}}\n{c5}\n"
    );
    let (status, v) = es(&es_addr, "POST", "/_bulk", Some(&unscoped)).await;
    assert_eq!((status, v["errors"].as_bool()), (200, Some(false)), "{v}");
    for i in [0, 1] {
        let it = &v["items"][i]["index"];
        assert_eq!((it["_index"].as_str(), it["result"].as_str()), (Some(INDEX), Some("created")), "{v}");
    }
    // `?op_type=create` on the bulk URL is not read (deviation): the
    // existing c2 is plainly re-indexed (200 updated), no conflict.
    let again = format!("{{\"index\":{{\"_id\":\"c2\"}}}}\n{c2}\n");
    let (status, v) = es(&es_addr, "POST", &format!("/{INDEX}/_bulk?op_type=create"), Some(&again)).await;
    assert_eq!((status, v["errors"].as_bool()), (200, Some(false)), "{v}");
    assert_eq!(v["items"][0]["index"]["result"], "updated", "{v}");

    // c) minimum_should_match over two should clauses (region=eu,
    //    vip=true): live docs c1 (eu+vip), c2, c4 (vip), c5 (eu).
    let both = search(&es_addr, &bool_query(&[], &[("region", "eu"), ("vip", "true")], Some("2"))).await;
    assert_eq!(both, (1, vec!["c1".to_string()]), "msm 2 = both clauses");
    let union = search(&es_addr, &bool_query(&[], &[("region", "eu"), ("vip", "true")], Some("1"))).await;
    assert_eq!(union, (3, vec!["c1".to_string(), "c4".to_string(), "c5".to_string()]), "msm 1 = union");
    assert_eq!(
        search(&es_addr, &bool_query(&[], &[("region", "eu"), ("vip", "true")], None)).await,
        union,
        "absent msm = union (should-only bool)"
    );
    assert_eq!(
        search(&es_addr, &bool_query(&[], &[("region", "eu"), ("vip", "true")], Some("\"75%\""))).await,
        both,
        "75% of 2 rounds to 2"
    );
    assert_eq!(
        search(&es_addr, &bool_query(&[], &[("region", "eu"), ("vip", "true")], Some("\"50%\""))).await,
        union,
        "50% of 2 = 1"
    );
    let gold_eu = search(&es_addr, &bool_query(&[("tier", "gold")], &[("region", "eu")], Some("1"))).await;
    assert_eq!(gold_eu, (1, vec!["c1".to_string()]), "explicit msm beside must gates the shoulds");
    let gold = search(&es_addr, &bool_query(&[("tier", "gold")], &[("region", "eu")], None)).await;
    assert_eq!(gold, (2, vec!["c1".to_string(), "c4".to_string()]), "no msm: should only scores");
    for bad in ["-1", "\"101%\"", "\"2<-25%\""] {
        let (status, v) = es(&es_addr, "POST", &format!("/{INDEX}/_search"), Some(&bool_query(&[], &[("region", "eu"), ("vip", "true")], Some(bad)))).await;
        assert_eq!(status, 400, "msm {bad}: {v}");
        assert_eq!(v["error"]["type"], "parsing_exception", "{v}");
        assert!(v["error"]["reason"].as_str().unwrap_or_default().contains("[minimum_should_match]"), "{v}");
    }
    assert_eq!(count(&es_addr, "{}").await, 4, "live customers: c1, c2, c4, c5");
    assert_eq!(count(&es_addr, &bool_query(&[], &[("region", "eu"), ("vip", "true")], None)).await, union.0);
    scenario_common::perf::within("upsert142/bulk-and-query", t, Duration::from_secs(2));

    // d) RESP keeps one hash per live customer; its tier/due cells
    //    are the ES document's and MySQL's.
    let live = [
        ("c1", "Ada", "gold", "120.00"),
        ("c2", "Ben", "silver", "45.00"),
        ("c4", "Dan", "gold", "200.00"),
        ("c5", "Eve", "bronze", "10.00"),
    ];
    for (id, name, tier, due) in live {
        let key = format!("cust:{id}");
        assert_eq!(
            int_of(
                &r(
                    &resp,
                    &[b"HSET", key.as_bytes(), b"name", name.as_bytes(), b"tier", tier.as_bytes(), b"due", due.as_bytes()]
                )
                .await
            ),
            3,
            "{id}"
        );
    }
    for (id, name, tier, due) in live {
        let key = format!("cust:{id}");
        assert_eq!(int_of(&r(&resp, &[b"HLEN", key.as_bytes()]).await), 3, "{id}");
        assert_eq!(rb(&resp, &[b"HGET", key.as_bytes(), b"tier"]).await, tier, "{id}");
        assert_eq!(rb(&resp, &[b"HGET", key.as_bytes(), b"due"]).await, due, "{id}");
        let (status, v) = es(&es_addr, "GET", &format!("/{INDEX}/_doc/{id}"), None).await;
        assert_eq!(status, 200, "{id}");
        assert_eq!(v["_source"]["tier"].as_str(), Some(tier), "{id}: RESP tier == ES tier");
        assert_eq!(v["_source"]["name"].as_str(), Some(name), "{id}");
        assert!(v["_source"]["due"].as_i64() == due.replace(".00", "").parse::<i64>().ok(), "{id}: {v}");
    }
    let hash = rf(&resp, &[b"HGETALL", b"cust:c5"]).await;
    for needle in ["name", "Eve", "tier", "bronze", "due", "10.00"] {
        assert!(hash.contains(needle), "{needle:?} in {hash}");
    }

    // e) MySQL keeps the billing rows; its counts / sums equal the ES
    //    term and range hits over the same customers.
    ddl(
        &mut c,
        "CREATE TABLE billing (customer VARCHAR(16) PRIMARY KEY, tier VARCHAR(8), due DECIMAL(10,2))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO billing VALUES ('c1', 'gold', 120.00), ('c2', 'silver', 45.00), \
         ('c4', 'gold', 200.00), ('c5', 'bronze', 10.00)",
    )
    .await;
    let gold_n = txt(&rows(&mut c, "SELECT COUNT(*) FROM billing WHERE tier = 'gold'").await[0][0]);
    let gold_es = search(&es_addr, r#"{"query":{"term":{"tier":"gold"}}}"#).await;
    assert_eq!(gold_n.parse::<i64>().unwrap(), gold_es.0, "SQL gold count == ES term hits");
    assert_eq!(gold_es.1, vec!["c1".to_string(), "c4".to_string()], "{gold_es:?}");
    let gold_sum = txt(&rows(&mut c, "SELECT SUM(due) FROM billing WHERE tier = 'gold'").await[0][0]);
    assert_eq!(gold_sum, "320.00");
    let (status, v) = es(&es_addr, "POST", &format!("/{INDEX}/_search?size=100"), Some(r#"{"query":{"term":{"tier":"gold"}}}"#)).await;
    assert_eq!(status, 200, "{v}");
    let es_sum: i64 = v["hits"]["hits"].as_array().map(|a| a.iter().map(|h| h["_source"]["due"].as_i64().unwrap_or(0)).sum()).unwrap_or(0);
    assert_eq!(es_sum, 320, "ES due sum == SQL SUM(due): {v}");
    let big: Vec<String> = rows(&mut c, "SELECT customer FROM billing WHERE due >= 100 ORDER BY customer").await.iter().map(|r| txt(&r[0])).collect();
    let big_es = search(&es_addr, r#"{"query":{"range":{"due":{"gte":100}}}}"#).await;
    assert_eq!((big_es.0, big_es.1), (big.len() as i64, big.clone()), "range due>=100 == SQL");
    assert_eq!(txt(&rows(&mut c, "SELECT COUNT(*) FROM billing").await[0][0]).parse::<i64>().unwrap(), count(&es_addr, "{}").await);

    scenario_common::perf::scenario("upsert142", t0);
}
