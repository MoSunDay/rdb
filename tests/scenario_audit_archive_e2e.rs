//! Multi-store scenario #20 -- a compliance audit archive across FOUR
//! fronts of one node: every review lands in SQL (the regulator's
//! ledger), rides the `audit/events` stream consumed by XREADGROUP and
//! acked once, is indexed into ES for exact term queries, and the
//! day's batch ships to S3 byte-identically; the reviewers' workload
//! board is a ZSET whose ZREVRANGE order must match the SQL per-actor
//! counts. Four planes (RESP + MySQL + ES + S3), one truth.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, http, json_of, ok, r, rb, rf, s3_call, stream_ids, FrontSpec};

#[tokio::test]
async fn audit_archive_four_fronts_one_truth() {
    // a) One node: MySQL + ES + S3 fronts; the ES index comes first.
    let spec = FrontSpec {
        mysql: true,
        es: true,
        s3: true,
        ..Default::default()
    };
    let (front, a) = front_world("audit20", &spec).await;
    let (es, s3) = (front.node.es.clone(), front.s3.clone());
    let mut c = connect_root(&front.node).await;
    let (status, body) = http(
        &es,
        "PUT",
        "/auditroom",
        Some(r#"{"mappings":{"properties":{"actor":{"type":"keyword"},"action":{"type":"keyword"},"doc":{"type":"keyword"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create auditroom index: {body}");
    ddl(
        &mut c,
        "CREATE TABLE audit_log (id BIGINT PRIMARY KEY, actor VARCHAR(8), action VARCHAR(8), doc VARCHAR(12))",
    )
    .await;

    // b) Six reviews by three auditors: SQL row + stream event + board
    //    bump + ES doc each. zed 3, amy 2, bo 1.
    let reviews = [
        ("zed", "view", "doc-a1"),
        ("amy", "view", "doc-b2"),
        ("zed", "delete", "doc-c3"),
        ("bo", "view", "doc-d4"),
        ("zed", "export", "doc-e5"),
        ("amy", "delete", "doc-f6"),
    ];
    for (i, (actor, action, doc)) in reviews.iter().enumerate() {
        run(&mut c, &format!(
            "INSERT INTO audit_log (id, actor, action, doc) VALUES ({}, '{actor}', '{action}', '{doc}')",
            i + 1
        ))
        .await;
        let ev = r(&a, &[b"XADD", b"audit/events", b"*",
                        b"actor", actor.as_bytes(), b"action", action.as_bytes(), b"doc", doc.as_bytes()]).await;
        assert!(scenario_common::bulk_at(&ev, 0).contains('-'), "event {doc}");
        r(&a, &[b"ZINCRBY", b"audit:load", b"1", actor.as_bytes()]).await;
        let es_doc = format!("{{\"actor\":\"{actor}\",\"action\":\"{action}\",\"doc\":\"{doc}\"}}");
        let (status, body) = http(&es, "PUT", &format!("/auditroom/_doc/{doc}"), Some(&es_doc)).await;
        assert_eq!(status, 201, "index {doc}: {body}");
    }

    // c) The reviewers group drains all six events exactly once.
    ok(&r(&a, &[b"XGROUP", b"CREATE", b"audit/events", b"reviewers", b"0-0"]).await, "xgroup");
    let batch = rf(
        &a,
        &[b"XREADGROUP", b"GROUP", b"reviewers", b"r1", b"COUNT", b"10", b"STREAMS", b"audit/events", b">"],
    )
    .await;
    for (_, _, doc) in reviews {
        assert!(batch.contains(doc), "{doc} delivered: {batch}");
    }
    let ids = stream_ids(&batch);
    assert_eq!(ids.len(), 6, "six events by id: {batch}");
    let mut ack_args: Vec<&[u8]> = vec![b"XACK", b"audit/events", b"reviewers"];
    ids.iter().for_each(|id| ack_args.push(id.as_bytes()));
    assert_eq!(scenario_common::int_of(&r(&a, &ack_args).await), 6, "all acked");
    let again = rf(
        &a,
        &[b"XREADGROUP", b"GROUP", b"reviewers", b"r1", b"STREAMS", b"audit/events", b">"],
    )
    .await;
    assert!(!again.contains("doc-"), "no redelivery: {again}");

    // d) The day's batch ships to S3 and comes back byte-identical
    //    (non-UTF8 bytes on purpose).
    let batch_bytes = b"\xff\xfe audit d1: 6 reviews \x00\x01";
    let (status, head, _) = s3_call(&s3, "PUT", "/audit/batch/d1.bin", batch_bytes).await;
    assert_eq!(status, 200, "s3 put: {head}");
    let (status, _, got) = s3_call(&s3, "GET", "/audit/batch/d1.bin", b"").await;
    assert_eq!(status, 200, "s3 get");
    assert_eq!(got, batch_bytes.to_vec(), "byte-exact archive");

    // e) The workload board: zed(3) > amy(2) > bo(1), ZSCOREs agree.
    let board = rf(&a, &[b"ZREVRANGE", b"audit:load", b"0", b"-1", b"WITHSCORES"]).await;
    assert!(board.find("zed").unwrap() < board.find("amy").unwrap(), "{board}");
    assert!(board.find("amy").unwrap() < board.find("bo").unwrap(), "{board}");
    assert_eq!(rb(&a, &[b"ZSCORE", b"audit:load", b"zed"]).await, "3");
    assert_eq!(rb(&a, &[b"ZSCORE", b"audit:load", b"amy"]).await, "2");
    assert_eq!(rb(&a, &[b"ZSCORE", b"audit:load", b"bo"]).await, "1");

    // f) ES term queries: exact keywords across actors and actions.
    let (status, _) = http(&es, "POST", "/auditroom/_refresh", None).await;
    assert_eq!(status, 200, "refresh");
    async fn hits(es: &str, q: &str) -> i64 {
        let (status, body) = http(es, "POST", "/auditroom/_search", Some(q)).await;
        assert_eq!(status, 200, "search: {body}");
        json_of(&body)["hits"]["total"]["value"].as_i64().unwrap_or(-1)
    }
    assert_eq!(hits(&es, r#"{"query":{"term":{"actor":"zed"}},"size":0}"#).await, 3, "zed's three");
    assert_eq!(hits(&es, r#"{"query":{"term":{"action":"delete"}},"size":0}"#).await, 2, "two deletes");
    assert_eq!(hits(&es, r#"{"query":{"term":{"doc":"doc-d4"}},"size":0}"#).await, 1, "one doc hit");

    // g) Reconciliation: SQL == board == ES == the acked stream.
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM audit_log").await, vec![6]);
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM audit_log WHERE actor = 'zed'").await,
        vec![3],
        "SQL agrees with ZSCORE zed"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM audit_log WHERE action = 'delete'").await,
        vec![2],
        "SQL agrees with the ES delete count"
    );
    assert_eq!(scenario_common::int_of(&r(&a, &[b"XLEN", b"audit/events"]).await), 6, "log retained");
}
