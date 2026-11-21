//! Multi-store scenario #56 -- CDN edge log shipping: three PoPs emit
//! access lines onto one RocksMQ channel, the shipper drains the batch
//! (b64-decoded bodies) and acks everything (pending gauge to zero),
//! each PoP's client bitmap merges into the global reach with BITOP OR
//! (the reply is the OR-map's popcount = distinct clients), the ES
//! face takes an explicit index + _bulk of the 5xx lines (refresh,
//! count, term hit), and a MySQL QoS ledger keeps per-PoP p95s. One
//! REAL node, RocksMQ + ES + RESP + SQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use common::mq::msgs_of;
use scenario_common::{codec, front_world, http, int_of, json_of, mq_post, r, FrontSpec};

#[tokio::test]
async fn edge_logs_ship_merge_and_reconcile() {
    let spec = FrontSpec {
        mysql: true,
        es: true,
        rocksmq: true,
        ..Default::default()
    };
    let (front, a) = front_world("cdn56", &spec).await;
    let (mq, es) = (front.rocksmq.clone(), front.node.es.clone());
    let mut c = connect_root(&front.node).await;

    // a) ES needs the index before any doc lands (no auto-create).
    let (status, body) = http(
        &es,
        "PUT",
        "/cdnlogs",
        Some(r#"{"mappings":{"properties":{"pop":{"type":"keyword"},"line":{"type":"text"},"status":{"type":"keyword"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create cdnlogs index: {body}");

    // b) Three PoPs emit five access lines onto one channel.
    let lines = [
        "tyo | GET /a.mp4 | 200",
        "fra | GET /b.js | 503",
        "ord | GET /a.mp4 | 200",
        "tyo | GET /c.css | 500",
        "ord | GET /d.png | 200",
    ];
    for line in lines {
        let (status, body) = mq_post(&mq, "/produce?channel=cdnacc", line).await;
        assert_eq!(status, 200, "produce {line}: {body}");
    }

    // c) The shipper drains the whole batch in one consume.
    let (status, body) = mq_post(&mq, "/consume?channel=cdnacc&group=shipper&n=10", "").await;
    assert_eq!(status, 200, "consume: {body}");
    let items: Vec<(String, String)> = msgs_of(&body)
        .into_iter()
        .map(|(id, b64)| (id, codec::b64d(&b64)))
        .collect();
    assert_eq!(items.len(), 5, "batch drained: {body}");

    // d) Client reach: each PoP saw a bitmap of client ids; the OR
    //    merge replies the global popcount (client 7 at two PoPs is
    //    still one client, client 9 at ord makes two).
    for (key, bit) in [("{cdn}ip:tyo", b"7"), ("{cdn}ip:fra", b"7"), ("{cdn}ip:ord", b"9")] {
        assert_eq!(int_of(&r(&a, &[b"SETBIT", key.as_bytes(), bit, b"1"]).await), 0, "{key} fresh");
    }
    assert_eq!(
        int_of(&r(&a, &[b"BITOP", b"OR", b"{cdn}reach", b"{cdn}ip:tyo", b"{cdn}ip:fra", b"{cdn}ip:ord"]).await),
        2,
        "OR popcount = distinct clients"
    );
    assert_eq!(int_of(&r(&a, &[b"BITCOUNT", b"{cdn}reach"]).await), 2);

    // e) The 5xx lines go to ES as one _bulk (errors must be false).
    let mut ndjson = String::new();
    let mut fives = 0usize;
    for (id, msg) in &items {
        if msg.contains("503") || msg.contains("500") {
            fives += 1;
            let pop = msg.split(" | ").next().unwrap_or("?");
            let doc = format!(
                "{{\"pop\":\"{pop}\",\"line\":{},\"status\":\"5xx\"}}",
                serde_json::to_string(msg).unwrap()
            );
            ndjson.push_str(&format!("{{\"index\":{{\"_index\":\"cdnlogs\",\"_id\":\"{id}\"}}}}\n"));
            ndjson.push_str(&doc);
            ndjson.push('\n');
        }
    }
    assert_eq!(fives, 2, "two 5xx lines");
    let (status, body) = http(&es, "POST", "/_bulk", Some(&ndjson)).await;
    assert_eq!(status, 200, "bulk: {body}");
    assert_eq!(json_of(&body)["errors"], serde_json::json!(false), "{body}");
    let (status, _) = http(&es, "POST", "/cdnlogs/_refresh", None).await;
    assert_eq!(status, 200, "refresh");

    // f) Term query: exactly one of the two 5xx lines came from fra.
    let q = r#"{"query":{"term":{"pop":"fra"}}}"#;
    let (_status, body) = http(&es, "POST", "/cdnlogs/_search", Some(q)).await;
    assert_eq!(json_of(&body)["hits"]["total"]["value"], serde_json::json!(1), "{body}");

    // g) Acks land; the pending gauge drops to zero.
    for (id, _) in &items {
        let (status, body) =
            mq_post(&mq, &format!("/ack?channel=cdnacc&group=shipper&id={id}"), "").await;
        assert_eq!(status, 200, "ack {id}: {body}");
    }
    assert_eq!(common::mq::pending_count(&mq, "cdnacc", "shipper").await, 0);

    // h) The MySQL QoS ledger reconciles the per-PoP p95s.
    ddl(
        &mut c,
        "CREATE TABLE pop_qos (pop VARCHAR(8) PRIMARY KEY, p95_ms INT NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO pop_qos (pop, p95_ms) VALUES ('tyo', 18), ('fra', 240), ('ord', 25)").await;
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM pop_qos WHERE p95_ms > 100").await, vec![1]);
}
