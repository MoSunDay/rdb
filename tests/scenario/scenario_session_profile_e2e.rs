//! Multi-store scenario #2 -- sessions and user profiles: the whole
//! account-surface state model over SIX storage classes in one REAL
//! node. String+TTL sessions (expire_e2e semantics), a Hash profile,
//! a JSON preferences snapshot (json_e2e wire shapes), a Set of who is
//! online, a List login trail kept bounded with LTRIM (list_e2e), the
//! expiry verdict after the deadline, and finally kill -9 + respawn:
//! every durable family survives, the expired session stays gone
//! (persistence semantics per lite_proc_e2e / process_failover_e2e).

use crate::common;
use crate::scenario_common;
use std::time::Duration;

use scenario_common::{front_world, int_of, ok, r, rb, rf, FrontSpec};

#[tokio::test]
async fn session_profile_families_and_restart_durability() {
    let _serial = scenario_common::serial_guard().await;
    // a) World: plain RESP node (no extra fronts -- the story is KV-only).
    let (mut front, a) = front_world("sess2", &FrontSpec::default()).await;

    // b) Sessions are short-lived strings: SET with EX, readable, TTL
    //    ticking inside the granted window.
    ok(
        &r(&a, &[b"SET", b"sess:t1", b"u1:web", b"EX", b"120"]).await,
        "set session",
    );
    assert_eq!(rb(&a, &[b"GET", b"sess:t1"]).await, "u1:web");
    let ttl = int_of(&r(&a, &[b"TTL", b"sess:t1"]).await);
    assert!((1..=120).contains(&ttl), "ttl in window, got {ttl}");

    // c) The profile is a Hash: name/city fields, whole-map read.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"HSET",
                    b"profile:u1",
                    b"name",
                    b"ada",
                    b"city",
                    b"guangzhou"
                ]
            )
            .await
        ),
        2,
        "hset profile",
    );
    let profile = rf(&a, &[b"HGETALL", b"profile:u1"]).await;
    assert!(
        profile.contains("ada") && profile.contains("guangzhou"),
        "{profile}"
    );
    assert_eq!(rb(&a, &[b"HGET", b"profile:u1", b"name"]).await, "ada");

    // d) Preferences are one JSON document at the root path.
    ok(
        &r(
            &a,
            &[
                b"JSON.SET",
                b"pref:u1",
                b"$",
                b"{\"theme\":\"dark\",\"lang\":\"zh\"}",
            ],
        )
        .await,
        "json.set prefs",
    );
    let pref = rb(&a, &[b"JSON.GET", b"pref:u1", b"$"]).await;
    assert!(pref.contains("dark") && pref.contains("zh"), "{pref}");

    // e) Presence is a Set: three online users, membership asks, no dup.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"online:lobby", b"u1", b"u2", b"u3"]).await),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"online:lobby", b"u2"]).await),
        0,
        "dup adds nothing"
    );
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"online:lobby"]).await), 3);
    assert_eq!(
        int_of(&r(&a, &[b"SISMEMBER", b"online:lobby", b"u2"]).await),
        1
    );
    let members = rf(&a, &[b"SMEMBERS", b"online:lobby"]).await;
    assert!(
        members.contains("u1") && members.contains("u3"),
        "{members}"
    );

    // f) The login trail is a List; LTRIM keeps the LAST five attempts.
    for ip in [
        "10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4", "10.0.0.5", "10.0.0.6",
    ] {
        int_of(&r(&a, &[b"RPUSH", b"trail:u1", ip.as_bytes()]).await);
    }
    ok(
        &r(&a, &[b"LTRIM", b"trail:u1", b"-5", b"-1"]).await,
        "ltrim trail",
    );
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"trail:u1"]).await), 5);
    let trail = rf(&a, &[b"LRANGE", b"trail:u1", b"0", b"-1"]).await;
    assert!(
        trail.contains("10.0.0.2") && !trail.contains("10.0.0.1"),
        "{trail}"
    );

    // g) Expiry verdict: a 1-second session dies within the poll budget;
    //    the durable families are untouched by its passing.
    ok(
        &r(&a, &[b"SET", b"sess:t2", b"u2:mobile", b"PX", b"800"]).await,
        "set short",
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if int_of(&r(&a, &[b"EXISTS", b"sess:t2"]).await) == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "short session never expired"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(rb(&a, &[b"GET", b"sess:t2"]).await, ""); // nil -> empty text
    assert_eq!(
        rb(&a, &[b"HGET", b"profile:u1", b"city"]).await,
        "guangzhou"
    );

    // h) kill -9 + respawn on the same data dir: the profile Hash, the
    //    JSON prefs, the presence Set and the trail List all survive
    //    (RocksDB persistence); the expired session is still gone.
    front.node.kill_now();
    front.node.respawn();
    common::wait_resp_ready(&mut front.node, 30).await;
    assert_eq!(rb(&a, &[b"HGET", b"profile:u1", b"name"]).await, "ada");
    let pref2 = rb(&a, &[b"JSON.GET", b"pref:u1", b"$"]).await;
    assert!(pref2.contains("dark"), "prefs survived: {pref2}");
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"online:lobby"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"trail:u1"]).await), 5);
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"sess:t2"]).await), 0);
    // The long-lived session survives too: TTL persistence carries the
    // deadline, and the test clock never approached 120s.
    assert_eq!(rb(&a, &[b"GET", b"sess:t1"]).await, "u1:web");
}
