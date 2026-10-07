//! Process-level e2e for the RocksMQ HTTP front's P3 surface:
//! `/produce_batch` + `/ack_batch` (per-item delegation, independent
//! items, mixed batches) and `/range` (id-ordered replay, bounds,
//! limit, read-only guarantee), plus the token gate on all three and
//! the `rocksmq_max_connections` cap. Fixture/client live in
//! `tests/common/mq.rs` (the factored rocksmq bootstrap).

mod common;

use common::mq::{
    b64, items_of, keepalive_round, msgs_of, node_up, pending_count, post, post_auth, req,
    speak_once,
};
use tokio::net::TcpStream;

/// FAKE bearer token for the gated node (never a real secret).
const MQ_TOKEN: &str = "e2e-fake-mq-token-0123456789abcdef";

fn batch_json(items: &[serde_json::Value]) -> String {
    serde_json::to_string(items).unwrap()
}

#[tokio::test]
async fn batch_produce_returns_ordered_ids() {
    let node = node_up("bp-ordered", "").await;
    // 5 items, distinct payloads; ids must come back ascending.
    let items: Vec<_> = (0..5)
        .map(|i| serde_json::json!({"channel": "bp1", "body": b64(format!("m{i}").as_bytes())}))
        .collect();
    let (status, body) = post(&node.http, "/produce_batch", &batch_json(&items)).await;
    assert_eq!(status, 200, "{body}");
    let results = items_of(&body);
    assert_eq!(results.len(), 5);
    let ids: Vec<&String> = results.iter().map(|(_, b)| b).collect();
    for (i, (s, id)) in results.iter().enumerate() {
        assert_eq!(*s, 200, "item {i}: {id}");
        assert!(id.contains('-'), "id shape: {id}");
    }
    assert!(ids[0] < ids[1] && ids[1] < ids[2] && ids[2] < ids[3] && ids[3] < ids[4]);

    // The messages are real: a group consume sees the same ids in order.
    let (s, b) = post(&node.http, "/consume?channel=bp1&group=g&n=5", "").await;
    assert_eq!(s, 200, "{b}");
    let got = msgs_of(&b);
    let got_ids: Vec<&String> = got.iter().map(|(id, _)| id).collect();
    assert_eq!(got_ids, ids, "consume vs batch ids");

    // Whole-request 400s: non-array body, non-JSON, over-cap array.
    for bad in ["{}", "not json", "[] extra"] {
        let (s, b) = post(&node.http, "/produce_batch", bad).await;
        assert_eq!(s, 400, "{bad}: {b}");
    }
    let big: Vec<_> = (0..101)
        .map(|_| serde_json::json!({"channel": "bp1"}))
        .collect();
    let (s, b) = post(&node.http, "/produce_batch", &batch_json(&big)).await;
    assert_eq!(s, 400, "{b}");
    // Empty array is legal and answers [].
    let (s, b) = post(&node.http, "/produce_batch", "[]").await;
    assert_eq!((s, b.as_str()), (200, "[]"));
}

#[tokio::test]
async fn batch_produce_mixed_invalid_item_is_isolated() {
    let node = node_up("bp-mixed", "").await;
    let items = [
        serde_json::json!({"channel": "bm1", "body": b64(b"ok0")}),
        serde_json::json!({"channel": "bad name", "body": b64(b"x")}), // charset refusal
        serde_json::json!({"body": b64(b"ok1")}),                      // missing channel
        serde_json::json!({"channel": "bm1", "body": "!!!not-base64!!!"}),
        serde_json::json!({"channel": "bm1", "body": b64(b"ok2"), "delay_ms": 0}),
    ];
    let (status, body) = post(&node.http, "/produce_batch", &batch_json(&items)).await;
    assert_eq!(status, 200, "{body}");
    let results = items_of(&body);
    assert_eq!(results.len(), 5, "{body}");
    assert_eq!(results[0].0, 200, "{:?}", results[0]);
    assert_eq!(results[1].0, 400);
    assert!(
        results[1].1.contains("invalid channel name"),
        "{:?}",
        results[1]
    );
    assert_eq!(results[2].0, 400);
    assert!(
        results[2].1.contains("missing 'channel' query parameter"),
        "{:?}",
        results[2]
    );
    assert_eq!(results[3].0, 400);
    assert!(results[3].1.contains("base64"), "{:?}", results[3]);
    assert_eq!(results[4].0, 200, "{:?}", results[4]);

    // The valid items landed despite the failing ones.
    let (s, b) = post(&node.http, "/consume?channel=bm1&group=g&n=10", "").await;
    assert_eq!(s, 200, "{b}");
    let got = msgs_of(&b);
    let bodies: Vec<&String> = got.iter().map(|(_, p)| p).collect();
    assert_eq!(bodies, [&b64(b"ok0"), &b64(b"ok2")]);
}

#[tokio::test]
async fn batch_ack_subset_shrinks_pending() {
    let node = node_up("ba-subset", "").await;
    let items: Vec<_> = (0..4)
        .map(|i| serde_json::json!({"channel": "ba1", "body": b64(format!("a{i}").as_bytes())}))
        .collect();
    let (s, b) = post(&node.http, "/produce_batch", &batch_json(&items)).await;
    assert_eq!(s, 200, "{b}");
    let ids: Vec<String> = items_of(&b).into_iter().map(|(_, id)| id).collect();

    let (s, b) = post(&node.http, "/consume?channel=ba1&group=g&n=4", "").await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(msgs_of(&b).len(), 4);
    assert_eq!(pending_count(&node.http, "ba1", "g").await, 4);

    // Ack a subset: ids[0] and ids[2].
    let acks = [
        serde_json::json!({"channel": "ba1", "group": "g", "id": ids[0]}),
        serde_json::json!({"channel": "ba1", "group": "g", "id": ids[2]}),
    ];
    let (s, b) = post(&node.http, "/ack_batch", &batch_json(&acks)).await;
    assert_eq!(s, 200, "{b}");
    let results = items_of(&b);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], (200, "ok".to_string()), "{results:?}");
    assert_eq!(results[1], (200, "ok".to_string()), "{results:?}");
    assert_eq!(pending_count(&node.http, "ba1", "g").await, 2);

    // Mixed ack batch: a garbage id is a per-item 400 next to a 200.
    let acks = [
        serde_json::json!({"channel": "ba1", "group": "g", "id": ids[1]}),
        serde_json::json!({"channel": "ba1", "group": "g", "id": "garbage"}),
        serde_json::json!({"channel": "ba1", "group": "nope", "id": ids[3]}),
    ];
    let (s, b) = post(&node.http, "/ack_batch", &batch_json(&acks)).await;
    assert_eq!(s, 200, "{b}");
    let results = items_of(&b);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].0, 200, "{results:?}");
    assert_eq!(results[1].0, 400);
    assert!(results[1].1.contains("invalid message id"), "{results:?}");
    assert_eq!(results[2].0, 404, "unknown group: {results:?}");
    assert!(
        results[2].1.contains("no such consumer group"),
        "{results:?}"
    );
    assert_eq!(pending_count(&node.http, "ba1", "g").await, 1);
}

#[tokio::test]
async fn range_replays_id_ordered_with_bounds_and_limit() {
    let node = node_up("br-bounds", "").await;
    let items: Vec<_> = (0..5)
        .map(|i| serde_json::json!({"channel": "br1", "body": b64(format!("m{i}").as_bytes())}))
        .collect();
    let (s, b) = post(&node.http, "/produce_batch", &batch_json(&items)).await;
    assert_eq!(s, 200, "{b}");
    let ids: Vec<String> = items_of(&b).into_iter().map(|(_, id)| id).collect();

    // Full replay, ascending ids, payloads intact.
    let (s, b) = post(&node.http, "/range?channel=br1", "").await;
    assert_eq!(s, 200, "{b}");
    let got = msgs_of(&b);
    let got_ids: Vec<&String> = got.iter().map(|(id, _)| id).collect();
    assert_eq!(got_ids, [&ids[0], &ids[1], &ids[2], &ids[3], &ids[4]]);
    let payloads: Vec<&String> = got.iter().map(|(_, p)| p).collect();
    assert_eq!(
        payloads,
        [
            &b64(b"m0"),
            &b64(b"m1"),
            &b64(b"m2"),
            &b64(b"m3"),
            &b64(b"m4")
        ]
    );

    // Exclusive begin / inclusive end.
    let target = format!("/range?channel=br1&begin=({}&end={}", ids[1], ids[3]);
    let (s, b) = post(&node.http, &target, "").await;
    assert_eq!(s, 200, "{b}");
    let got_ids: Vec<String> = msgs_of(&b).into_iter().map(|(id, _)| id).collect();
    assert_eq!(
        got_ids,
        vec![ids[2].clone(), ids[3].clone()],
        "exclusive begin, inclusive end"
    );

    // limit caps the page from the begin bound.
    let (s, b) = post(&node.http, "/range?channel=br1&limit=2", "").await;
    assert_eq!(s, 200, "{b}");
    let got_ids: Vec<String> = msgs_of(&b).into_iter().map(|(id, _)| id).collect();
    assert_eq!(got_ids, vec![ids[0].clone(), ids[1].clone()]);

    // topic= alias; unknown channel; bad bounds; GET -> 405.
    let (s, b) = post(&node.http, "/range?topic=br1&limit=1", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 1), "{b}");
    let (s, b) = post(&node.http, "/range?channel=never-existed", "").await;
    assert_eq!((s, b.as_str()), (200, r#"{"msgs":[]}"#));
    for bad in ["begin=x", "end=1", "begin=(-", "limit=0", "limit=x"] {
        let (s, b) = post(&node.http, &format!("/range?channel=br1&{bad}"), "").await;
        assert_eq!(s, 400, "{bad}: {b}");
    }
    let (s, _, b) = req(&node.http, "GET", "/range?channel=br1", "", &[]).await;
    assert_eq!((s, b.as_str()), (405, "method not allowed"));
}

#[tokio::test]
async fn range_is_read_only() {
    let node = node_up("br-readonly", "").await;
    let items: Vec<_> = (0..3)
        .map(|i| serde_json::json!({"channel": "ro1", "body": b64(format!("r{i}").as_bytes())}))
        .collect();
    let (s, b) = post(&node.http, "/produce_batch", &batch_json(&items)).await;
    assert_eq!(s, 200, "{b}");

    // Replay twice; no group appears (/pending stays 404), nothing is
    // consumed (the entries stay replayable).
    for _ in 0..2 {
        let (s, b) = post(&node.http, "/range?channel=ro1", "").await;
        assert_eq!((s, msgs_of(&b).len()), (200, 3), "{b}");
    }
    let (s, b) = post(&node.http, "/pending?channel=ro1&group=g", "").await;
    assert_eq!((s, b.as_str()), (404, "no such consumer group"));

    // A group consume AFTER the replays still delivers every unacked
    // message, and the PEL counts exactly what was delivered -- /range
    // created no deliveries of its own.
    let (s, b) = post(&node.http, "/consume?channel=ro1&group=g&n=10", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 3), "{b}");
    assert_eq!(pending_count(&node.http, "ro1", "g").await, 3);
    let (s, b) = post(&node.http, "/range?channel=ro1", "").await;
    assert_eq!((s, msgs_of(&b).len()), (200, 3), "{b}");
    assert_eq!(pending_count(&node.http, "ro1", "g").await, 3);
}

#[tokio::test]
async fn token_gate_covers_all_three_new_routes() {
    let yaml = format!("rocksmq_token: \"{MQ_TOKEN}\"\n");
    let node = node_up("gate", &yaml).await;
    let bearer = format!("Bearer {MQ_TOKEN}");
    for target in ["/produce_batch", "/ack_batch", "/range?channel=g1"] {
        let (s, b) = post(&node.http, target, "[{\"channel\":\"g1\"}]").await;
        assert_eq!((s, b.as_str()), (401, "unauthorized"), "{target}");
    }
    // With the bearer, all three answer their normal 200s.
    let (s, b) = post_auth(&node.http, "/produce_batch", "[]", &bearer).await;
    assert_eq!((s, b.as_str()), (200, "[]"), "{b}");
    let (s, b) = post_auth(&node.http, "/ack_batch", "[]", &bearer).await;
    assert_eq!((s, b.as_str()), (200, "[]"), "{b}");
    let (s, b) = post_auth(&node.http, "/range?channel=g1", "", &bearer).await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b, r#"{"msgs":[]}"#);
}

#[tokio::test]
async fn connection_cap_refuses_second_concurrent_connection() {
    let node = node_up("cap", "rocksmq_max_connections: 1\n").await;
    // Hold the one slot: a keep-alive connection that already answered
    // (retry past the readiness probe's own transient slot).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let a = loop {
        assert!(
            std::time::Instant::now() < deadline,
            "never won the cap slot"
        );
        let mut sock = TcpStream::connect(&node.http).await.expect("connect A");
        if let Some((status, _)) = keepalive_round(&mut sock, "/range?channel=cap1").await {
            assert_eq!(status, 200);
            break sock;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    // A second concurrent connection is refused: closed without one
    // reply byte (the kafka-front posture, no HTTP error page).
    match speak_once(&node.http, "/range?channel=cap1").await {
        None => {}
        Some((status, _)) => panic!("second connection answered {status} at cap 1"),
    }
    // Releasing the slot (close A) admits the next connection again.
    drop(a);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (status, _) = speak_once(&node.http, "/range?channel=cap1")
        .await
        .expect("admitted after release");
    assert_eq!(status, 200);
}
