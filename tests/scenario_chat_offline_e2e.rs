//! Multi-store scenario #13 -- a chat service: presence is a `SET NX PX`
//! session lock (second lock of the same user is nil), profiles are
//! Hashes, message fan-out rides a lite stream consumed by XREADGROUP --
//! online users are acked in place, offline users get an LPUSH spool --
//! receipts use GETDEL (first read bulk, second nil), every message is
//! archived to SQL, and the ES front indexes the transcript for exact
//! term queries (keyword terms are case-sensitive).

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, http, int_of, json_of, ok, r, rb, rf, stream_ids, FrontSpec};

#[tokio::test]
async fn chat_offline_spool_receipts_and_es_transcript() {
    // a) One node: MySQL + ES fronts; the ES index must exist first.
    let spec = FrontSpec {
        mysql: true,
        es: true,
        ..Default::default()
    };
    let (front, a) = front_world("chat13", &spec).await;
    let es = front.node.es.clone();
    let mut c = connect_root(&front.node).await;
    let (status, body) = http(
        &es,
        "PUT",
        "/chatroom",
        Some(r#"{"mappings":{"properties":{"user":{"type":"keyword"},"text":{"type":"keyword"}}}}"#),
    )
    .await;
    assert_eq!(status, 200, "create chatroom index: {body}");
    ddl(
        &mut c,
        "CREATE TABLE chat_archive (msg VARCHAR(32) PRIMARY KEY, target VARCHAR(16), offline INT)",
    )
    .await;

    // b) Presence: u1/u2 take session locks; u1's re-lock is refused
    //    (nil bulk); u3 never connects.
    ok(&r(&a, &[b"SET", b"chat:online:u1", b"sess", b"NX", b"PX", b"60000"]).await, "u1 locks in");
    let relock = r(&a, &[b"SET", b"chat:online:u1", b"sess2", b"NX", b"PX", b"60000"]).await;
    assert!(relock.starts_with(b"$-1"), "double session refused: {relock:?}");
    ok(&r(&a, &[b"SET", b"chat:online:u2", b"sess", b"NX", b"PX", b"60000"]).await, "u2 locks in");
    assert_eq!(rb(&a, &[b"GET", b"chat:online:u3"]).await, "", "u3 has no lock");

    // c) Profiles: u3's Hash reads back whole via HGETALL.
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"chat:profile:u3", b"nick", b"carol", b"city", b"hz", b"tz", b"8"]).await),
        3
    );
    let profile = rf(&a, &[b"HGETALL", b"chat:profile:u3"]).await;
    assert!(profile.contains("carol") && profile.contains("hz") && profile.contains("tz"), "{profile}");

    // d) Three messages ride the room stream: two to online u1, one to
    //    offline u3.
    let msgs = [("m1", "u1", "PingBack"), ("m2", "u3", "hello there"), ("m3", "u1", "PingPong")];
    for (m, to, text) in msgs {
        let ev = r(&a, &[b"XADD", b"chat/room", b"*", b"id", m.as_bytes(), b"to", to.as_bytes(), b"text", text.as_bytes()]).await;
        assert!(scenario_common::bulk_at(&ev, 0).contains('-'), "xadd {m}");
    }

    // e) The fan-out worker: one delivery at a time; online targets are
    //    acked in place, the offline target is spooled via LPUSH, and
    //    every message is indexed into ES + archived into SQL.
    ok(&r(&a, &[b"XGROUP", b"CREATE", b"chat/room", b"fanout", b"0-0"]).await, "xgroup");
    for (m, to, text) in msgs {
        let one = rf(
            &a,
            &[b"XREADGROUP", b"GROUP", b"fanout", b"w1", b"COUNT", b"1", b"STREAMS", b"chat/room", b">"],
        )
        .await;
        assert!(one.contains(m), "delivered {m}: {one}");
        let id = stream_ids(&one)[0].clone();
        if rb(&a, &[b"GET", &format!("chat:online:{to}").into_bytes()]).await.is_empty() {
            assert_eq!(to, "u3", "only u3 is offline");
            assert_eq!(int_of(&r(&a, &[b"LPUSH", b"chat:offline:u3", m.as_bytes()]).await), 1, "spooled");
            run(&mut c, &format!("INSERT INTO chat_archive (msg, target, offline) VALUES ('{m}', '{to}', 1)")).await;
        } else {
            run(&mut c, &format!("INSERT INTO chat_archive (msg, target, offline) VALUES ('{m}', '{to}', 0)")).await;
        }
        let doc = format!("{{\"user\":\"{to}\",\"text\":\"{text}\"}}");
        let (status, body) = http(&es, "PUT", &format!("/chatroom/_doc/{m}"), Some(&doc)).await;
        assert_eq!(status, 201, "index {m}: {body}");
        assert_eq!(int_of(&r(&a, &[b"XACK", b"chat/room", b"fanout", id.as_bytes()]).await), 1, "ack {m}");
    }
    let drained = rf(
        &a,
        &[b"XREADGROUP", b"GROUP", b"fanout", b"w1", b"STREAMS", b"chat/room", b">"],
    )
    .await;
    assert!(!drained.contains("m1") && !drained.contains("m3"), "no redelivery: {drained}");

    // f) The offline spool holds exactly u3's message.
    let spool = rf(&a, &[b"LRANGE", b"chat:offline:u3", b"0", b"-1"]).await;
    assert!(spool.contains("m2") && !spool.contains("m1"), "{spool}");
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"chat:offline:u3"]).await), 1);

    // g) Read receipts: the worker deposits one and burns it with
    //    GETDEL -- first read is the bulk value, the second is nil.
    ok(&r(&a, &[b"SET", b"chat:rcpt:m1", b"seen"]).await, "receipt written");
    assert_eq!(rb(&a, &[b"GETDEL", b"chat:rcpt:m1"]).await, "seen", "first GETDEL returns it");
    assert_eq!(rb(&a, &[b"GETDEL", b"chat:rcpt:m1"]).await, "", "second GETDEL is nil");

    // h) ES transcript: refresh, then exact term queries. Keyword terms
    //    are case-sensitive: "u1" matches, "U1" never does.
    let (status, _) = http(&es, "POST", "/chatroom/_refresh", None).await;
    assert_eq!(status, 200, "refresh");
    async fn hits(es: &str, q: &str) -> i64 {
        let (status, body) = http(es, "POST", "/chatroom/_search", Some(q)).await;
        assert_eq!(status, 200, "search: {body}");
        json_of(&body)["hits"]["total"]["value"].as_i64().unwrap_or(-1)
    }
    assert_eq!(hits(&es, r#"{"query":{"term":{"user":"u1"}},"size":0}"#).await, 2, "u1's two texts");
    assert_eq!(hits(&es, r#"{"query":{"term":{"user":"U1"}},"size":0}"#).await, 0, "terms are case-sensitive");
    assert_eq!(hits(&es, r#"{"query":{"term":{"text":"hello there"}},"size":0}"#).await, 1, "u3's text indexed");

    // i) SQL archive reconciliation: three messages, one offline.
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM chat_archive").await, vec![3]);
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM chat_archive WHERE offline = 1").await,
        vec![1],
        "exactly u3 was spooled"
    );
}
