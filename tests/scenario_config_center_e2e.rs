//! Multi-store scenario #14 -- a configuration center: the live doc is
//! a JSON string (JSON.SET / JSON.GET / JSON.NUMINCRBY on `.rev`), the
//! publish epoch is an INCR counter, services are discoverable through
//! a full-text index (FT.CREATE + FT.ADD json docs + FT.SEARCH,
//! social_feed SCHEMA shape), every change lands in a SQL audit table,
//! point-in-time snapshots ship through the S3 front (PUT then GET,
//! byte-exact, media_archive shapes), and reads ride a SET PX cache
//! whose TTL is reshaped by EXPIRE then removed by PERSIST.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, ok, r, rb, rf, s3_call, FrontSpec};

#[tokio::test]
async fn config_center_json_search_snapshot_and_cache() {
    // a) One node: MySQL + S3 fronts.
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, a) = front_world("cfg14", &spec).await;
    let s3 = front.s3.clone();
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE cfg_audit (seq BIGINT PRIMARY KEY, what VARCHAR(16), detail VARCHAR(64))",
    )
    .await;

    // b) The live doc: seed rev 7, bump it twice through the JSON plane.
    ok(
        &r(&a, &[b"JSON.SET", b"cfg:doc", b".", br#"{"app":"pay-svc","rev":7,"tier":"gold"}"#]).await,
        "json.set root",
    );
    let doc = rb(&a, &[b"JSON.GET", b"cfg:doc", b"."]).await;
    assert!(doc.contains("pay-svc") && doc.contains("7"), "{doc}");
    assert_eq!(bulk_at(&r(&a, &[b"JSON.NUMINCRBY", b"cfg:doc", b".rev", b"1"]).await, 0), "8", "rev 8");
    assert_eq!(bulk_at(&r(&a, &[b"JSON.NUMINCRBY", b"cfg:doc", b".rev", b"1"]).await, 0), "9", "rev 9");
    run(&mut c, "INSERT INTO cfg_audit (seq, what, detail) VALUES (1, 'json', 'rev 7->9')").await;

    // c) The publish epoch ticks with every change (INCR).
    assert_eq!(int_of(&r(&a, &[b"INCR", b"cfg:epoch"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"INCR", b"cfg:epoch"]).await), 2);
    run(&mut c, "INSERT INTO cfg_audit (seq, what, detail) VALUES (2, 'epoch', 'tick 2')").await;

    // d) Service discovery: a text index over service docs.
    ok(
        &r(&a, &[b"FT.CREATE", b"cfg:idx", b"SCHEMA", b"app", b"TEXT", b"tier", b"TEXT", b"note", b"TEXT"]).await,
        "ft.create",
    );
    let services = [
        ("svc-pay", r#"{"app":"pay-svc","tier":"gold","note":"payments core"}"#),
        ("svc-chat", r#"{"app":"chat-svc","tier":"gold","note":"im core"}"#),
        ("svc-media", r#"{"app":"media-svc","tier":"free","note":"transcode"}"#),
    ];
    for (doc_id, json) in services {
        assert_eq!(
            int_of(&r(&a, &[b"FT.ADD", b"cfg:idx", doc_id.as_bytes(), json.as_bytes()]).await),
            1,
            "ft.add {doc_id}"
        );
    }
    let gold = rf(&a, &[b"FT.SEARCH", b"cfg:idx", b"@tier:gold"]).await;
    assert!(gold.contains(":2\r\n"), "two gold services: {gold:?}");
    assert!(gold.contains("svc-pay") && gold.contains("svc-chat"), "{gold:?}");
    let tc = rf(&a, &[b"FT.SEARCH", b"cfg:idx", b"@note:transcode"]).await;
    assert!(tc.contains(":1\r\n") && tc.contains("svc-media"), "{tc:?}");
    run(&mut c, "INSERT INTO cfg_audit (seq, what, detail) VALUES (3, 'ft', '3 services indexed')").await;

    // e) Snapshot: the published state ships through S3 and comes back
    //    byte-identical (body carries non-UTF8 bytes on purpose).
    let snap = b"\xff\xfe cfg-snap rev=9 epoch=2 \x00\x01";
    let (status, head, _) = s3_call(&s3, "PUT", "/cfg/snap/v2.bin", snap).await;
    assert_eq!(status, 200, "s3 put: {head}");
    let (status, _, got) = s3_call(&s3, "GET", "/cfg/snap/v2.bin", b"").await;
    assert_eq!(status, 200, "s3 get");
    assert_eq!(got, snap.to_vec(), "byte-exact snapshot round trip");

    // f) Read cache: born with a PX TTL, reshaped by EXPIRE, then made
    //    persistent by PERSIST (TTL -> -1, key still EXISTS).
    ok(&r(&a, &[b"SET", b"cfg:cache:v9", b"blob", b"PX", b"60000"]).await, "cache with ttl");
    let ttl = int_of(&r(&a, &[b"TTL", b"cfg:cache:v9"]).await);
    assert!((1..=60).contains(&(ttl / 1000)) || (1..=60000).contains(&ttl), "px ttl shape: {ttl}");
    assert_eq!(int_of(&r(&a, &[b"EXPIRE", b"cfg:cache:v9", b"300"]).await), 1, "expire reshapes ttl");
    let ttl = int_of(&r(&a, &[b"TTL", b"cfg:cache:v9"]).await);
    assert!((1..=300).contains(&ttl), "seconds ttl: {ttl}");
    assert_eq!(int_of(&r(&a, &[b"PERSIST", b"cfg:cache:v9"]).await), 1, "persist drops the ttl");
    assert_eq!(int_of(&r(&a, &[b"TTL", b"cfg:cache:v9"]).await), -1, "no ttl left");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"cfg:cache:v9"]).await), 1, "still there");

    // g) Reconciliation: the live doc holds rev 9 + epoch 2, the audit
    //    table saw every change, and the snapshot matches them.
    let live = rb(&a, &[b"JSON.GET", b"cfg:doc", b"."]).await;
    assert!(live.contains("9"), "{live}");
    assert_eq!(rb(&a, &[b"GET", b"cfg:epoch"]).await, "2");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM cfg_audit").await, vec![3]);
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM cfg_audit WHERE what IN ('json','epoch','ft')").await,
        vec![3],
        "every change audited"
    );
}
