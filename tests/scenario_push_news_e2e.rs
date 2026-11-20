//! Multi-store scenario #18 -- a news push service over THREE fronts of
//! one node: producers publish through the Kafka front (hand-framed
//! Produce v3 with record headers, decoded into k/v/h stream fields per
//! src/kafka/produce.rs), the fan-out worker consumes the SAME stream
//! over RESP with XREADGROUP -- per delivery it updates a Hash profile,
//! bumps a ZINCRBY watermark, archives to SQL and indexes into ES --
//! then acks by minted id. The wire truth: seed + 3 produced entries,
//! base_offset == pre-append len, headers ride hex-named JSON.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{front_world, http, json_of, ok, r, rf, stream_ids, FrontSpec};

#[tokio::test]
async fn push_news_kafka_uplink_resp_fanout_es_index() {
    // a) One node: Kafka + MySQL + ES fronts; index before docs.
    let spec = FrontSpec {
        mysql: true,
        es: true,
        kafka: true,
        ..Default::default()
    };
    let (front, a) = front_world("news18", &spec).await;
    let (kafka, es) = (front.kafka.clone(), front.node.es.clone());
    let mut c = connect_root(&front.node).await;
    let (status, body) = http(
        &es,
        "PUT",
        "/newswire",
        Some(r#"{"mappings":{"properties":{"user":{"type":"keyword"},"headline":{"type":"keyword"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create newswire index: {body}");
    ddl(
        &mut c,
        "CREATE TABLE news_deliveries (id BIGINT PRIMARY KEY, user VARCHAR(8), headline VARCHAR(32))",
    )
    .await;

    // b) RESP pre-seeds the topic's queue (topic `news` -> stream
    //    news/q0), then three records ride Produce v3 with headers.
    let seeded = r(&a, &[b"XADD", b"news/q0", b"1-1", b"seed", b"boot"]).await;
    assert!(scenario_common::bulk_at(&seeded, 0).contains("1-1"), "seed echo");
    let news = [
        ("u1", "market-rally"),
        ("u2", "storm-warning"),
        ("u1", "sport-final"),
    ];
    let records: Vec<rdb::kafka::record::BatchRecord<'_>> = news
        .iter()
        .map(|(user, headline)| rdb::kafka::record::BatchRecord {
            timestamp_delta: 0,
            key: Some(user.as_bytes()),
            value: Some(headline.as_bytes()),
            headers: vec![(b"lang".as_slice(), Some(b"en".as_slice()))],
        })
        .collect();
    let base = kafka_produce_v3(&kafka, 11, "news", &records).await;
    assert_eq!(base, 1, "base_offset = the seed entry");

    // c) Mutual proof on RESP: four entries, bodies + header fields.
    assert_eq!(scenario_common::int_of(&r(&a, &[b"XLEN", b"news/q0"]).await), 4);
    let xrange = rf(&a, &[b"XRANGE", b"news/q0", b"-", b"+"]).await;
    for (_, headline) in news {
        assert!(xrange.contains(headline), "{headline} visible: {xrange}");
    }
    assert!(xrange.contains("6c616e67"), "header name 'lang' hex-encoded: {xrange}");

    // d) Fan-out: one delivery at a time; the seed boot entry is acked
    //    without fan-out, each news entry fans out to four stores.
    ok(&r(&a, &[b"XGROUP", b"CREATE", b"news/q0", b"mailers", b"0-0"]).await, "xgroup");
    let mut delivered = 0i64;
    for _ in 0..4 {
        let one = rf(
            &a,
            &[b"XREADGROUP", b"GROUP", b"mailers", b"w1", b"COUNT", b"1", b"STREAMS", b"news/q0", b">"],
        )
        .await;
        let id = stream_ids(&one)[0].clone();
        if let Some((user, headline)) = news.iter().find(|(_, h)| one.contains(h)) {
            delivered += 1;
            let user_key = format!("news:profile:{user}");
            let added = scenario_common::int_of(
                &r(&a, &[b"HSET", user_key.as_bytes(), b"last", headline.as_bytes()]).await
            );
            assert!(added <= 1, "HSET counts NEW fields only: {added}");
            r(&a, &[b"ZINCRBY", b"news:wm", b"1", user.as_bytes()]).await;
            run(&mut c, &format!(
                "INSERT INTO news_deliveries (id, user, headline) VALUES ({delivered}, '{user}', '{headline}')"
            ))
            .await;
            let doc = format!("{{\"user\":\"{user}\",\"headline\":\"{headline}\"}}");
            let (status, body) = http(&es, "PUT", &format!("/newswire/_doc/{headline}"), Some(&doc)).await;
            assert_eq!(status, 201, "index {headline}: {body}");
        } else {
            assert!(one.contains("boot"), "the only skipped entry is the seed: {one}");
        }
        assert_eq!(
            scenario_common::int_of(&r(&a, &[b"XACK", b"news/q0", b"mailers", id.as_bytes()]).await),
            1,
            "acked {id}"
        );
    }
    assert_eq!(delivered, 3, "three news entries fanned out");
    let drained = rf(
        &a,
        &[b"XREADGROUP", b"GROUP", b"mailers", b"w1", b"STREAMS", b"news/q0", b">"],
    )
    .await;
    assert!(!drained.contains("market-rally"), "no redelivery: {drained}");

    // e) Watermarks: u1 got 2, u2 got 1 -- ZREVRANGE ranks u1 first.
    let wm = rf(&a, &[b"ZREVRANGE", b"news:wm", b"0", b"-1", b"WITHSCORES"]).await;
    assert!(wm.find("u1").unwrap() < wm.find("u2").unwrap(), "{wm}");
    assert_eq!(scenario_common::rb(&a, &[b"ZSCORE", b"news:wm", b"u1"]).await, "2");
    assert_eq!(scenario_common::rb(&a, &[b"ZSCORE", b"news:wm", b"u2"]).await, "1");
    // u1's profile holds its LATEST headline (sport-final delivered last).
    let profile = rf(&a, &[b"HGETALL", b"news:profile:u1"]).await;
    assert!(profile.contains("sport-final"), "{profile}");

    // f) ES index reachable: term queries on exact keywords.
    let (status, _) = http(&es, "POST", "/newswire/_refresh", None).await;
    assert_eq!(status, 200, "refresh");
    async fn hits(es: &str, q: &str) -> i64 {
        let (status, body) = http(es, "POST", "/newswire/_search", Some(q)).await;
        assert_eq!(status, 200, "search: {body}");
        json_of(&body)["hits"]["total"]["value"].as_i64().unwrap_or(-1)
    }
    assert_eq!(hits(&es, r#"{"query":{"term":{"user":"u1"}},"size":0}"#).await, 2);
    assert_eq!(hits(&es, r#"{"query":{"term":{"headline":"storm-warning"}},"size":0}"#).await, 1);

    // g) SQL reconciliation: three deliveries, two for u1, one for u2.
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM news_deliveries").await, vec![3]);
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM news_deliveries WHERE user = 'u1'").await,
        vec![2],
        "SQL matches the watermark"
    );
    assert_eq!(scenario_common::int_of(&r(&a, &[b"XLEN", b"news/q0"]).await), 4, "log retained");
}
