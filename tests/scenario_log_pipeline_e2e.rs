//! Multi-store scenario #5 -- a log shipping pipeline across THREE
//! transports: an app batch-writes raw lines through the RocksMQ HTTP
//! front (rocksmq_http_e2e /produce+/consume+/ack), the shipper
//! forwards ERROR lines into the ES front (`_doc` writes), the ES
//! `_search` count must match, and a SQL aggregation table keeps the
//! per-source totals for reconciliation. One REAL node, three fronts.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, one, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, http, json_of, mq_post, FrontSpec};

#[tokio::test]
async fn log_pipeline_rocksmq_es_and_sql_reconcile() {
    // a) One node: RocksMQ HTTP + ES HTTP + MySQL fronts together.
    let spec = FrontSpec {
        mysql: true,
        es: true,
        rocksmq: true,
        ..Default::default()
    };
    let (front, a) = front_world("logs5", &spec).await;
    let (mq, es) = (front.rocksmq.clone(), front.node.es.clone());
    let mut c = connect_root(&front.node).await;

    // b0) ES needs the index before any doc lands (no auto-create).
    let (status, body) = http(
        &es,
        "PUT",
        "/applog",
        Some(r#"{"mappings":{"properties":{"source":{"type":"keyword"},"line":{"type":"text"},"level":{"type":"keyword"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create applog index: {body}");

    // b) The app produces two batches of log lines onto one channel.
    let lines = [
        "api | INFO  | GET /health ok",
        "api | ERROR | db timeout on /checkout",
        "wk  | INFO  | job 17 done",
        "wk  | ERROR | job 18 crashed",
        "api | INFO  | GET /cart ok",
    ];
    for line in lines {
        let (status, body) = mq_post(&mq, "/produce?channel=applog", line).await;
        assert_eq!(status, 200, "produce {line}: {body}");
    }

    // c) The shipper group drains all five, forwards the ERROR ones,
    //    and acks everything it saw.
    let (status, body) = mq_post(&mq, "/consume?channel=applog&group=shipper&n=10", "").await;
    assert_eq!(status, 200, "consume: {body}");
    let items: Vec<(String, String)> = common::mq::msgs_of(&body)
        .into_iter()
        .map(|(id, b64)| (id, scenario_common::codec::b64d(&b64)))
        .collect();
    assert_eq!(items.len(), 5, "all lines drained: {body}");
    let mut errors = 0usize;
    for (id, msg) in &items {
        assert!(lines.iter().any(|l| l == msg), "unknown msg {msg:?}");
        if msg.contains("ERROR") {
            errors += 1;
            let src = msg.split(" | ").next().unwrap_or("?");
            let doc = format!(
                "{{\"source\":\"{src}\",\"line\":{},\"level\":\"ERROR\"}}",
                serde_json::to_string(msg).unwrap()
            );
            let (status, body) = http(&es, "PUT", &format!("/applog/_doc/{id}"), Some(&doc)).await;
            assert_eq!(status, 201, "index {id}: {body}");
        }
    }
    assert_eq!(errors, 2, "exactly two ERROR lines");
    for (id, _) in &items {
        let (status, body) =
            mq_post(&mq, &format!("/ack?channel=applog&group=shipper&id={id}"), "").await;
        assert_eq!(status, 200, "ack {id}: {body}");
    }
    // Fully acked: the pending gauge drops to zero.
    let pending = common::mq::pending_count(&mq, "applog", "shipper").await;
    assert_eq!(pending, 0, "shipper acked everything");

    // d) The ES side must find both ERROR docs (refresh, then count).
    let (status, _) = http(&es, "POST", "/applog/_refresh", None).await;
    assert_eq!(status, 200, "refresh");
    let (status, body) = http(
        &es,
        "POST",
        "/applog/_search",
        Some(r#"{"query":{"term":{"level":"ERROR"}},"size":0}"#),
    )
    .await;
    assert_eq!(status, 200, "search: {body}");
    let hits = json_of(&body)
        .pointer("/hits/total/value")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    assert_eq!(hits, 2, "exactly the two ERROR lines");

    // e) SQL ledger: one row per source; totals reconcile with the
    //    pipeline's ground truth (5 shipped, 2 errors).
    ddl(
        &mut c,
        "CREATE TABLE ship_stats (source VARCHAR(16) PRIMARY KEY, shipped INT, errors INT)",
    )
    .await;
    run(&mut c, "INSERT INTO ship_stats (source, shipped, errors) VALUES ('api', 3, 1), ('wk', 2, 1)").await;
    let total = one(&mut c, "SELECT SUM(shipped) FROM ship_stats").await;
    let err_total = one(&mut c, "SELECT SUM(errors) FROM ship_stats").await;
    let n = |v: &MVal| match v {
        MVal::Int(i) => *i,
        MVal::Bytes(b) => String::from_utf8_lossy(b).parse().unwrap(),
        other => panic!("cell {other:?}"),
    };
    assert_eq!(n(&total), 5, "SQL total == produced lines");
    assert_eq!(n(&err_total), 2, "SQL errors == ES hits");
    // The RESP side can see the stream the HTTP front wrote into.
    assert_eq!(
        scenario_common::int_of(&scenario_common::r(&a, &[b"XLEN", b"applog/q0"]).await),
        5,
        "rocksmq channel applog lives on the stream applog/q0"
    );
}
