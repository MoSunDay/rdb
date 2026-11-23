//! Multi-store scenario #62 -- a URL shortener back office: SETNX
//! claims a vanity slug exactly once, INR mints ids, GETDEL burns a
//! one-time preview token, SETEX caches a redirect hotspot whose TTL
//! PERSIST outlives, SETRANGE/GETRANGE patch and slice the domain
//! whitelist, and a hash holding the same name refuses GET with
//! WRONGTYPE. MySQL keeps the slug->target registry. One REAL node,
//! RESP + MySQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, one, run, s};
use scenario_common::{front_world, int_of, ok, r, rb, rt, FrontSpec};

#[tokio::test]
async fn slug_claims_tokens_and_whitelist_patch() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("short62", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Vanity slug claims: first SETNX wins, the loser keeps the
    //    generated fallback.
    assert_eq!(
        int_of(&r(&a, &[b"SETNX", b"{su}slug:home", b"https://acme.io"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETNX", b"{su}slug:home", b"https://evil.xx"]).await),
        0,
        "claimed once"
    );
    assert_eq!(rb(&a, &[b"GET", b"{su}slug:home"]).await, "https://acme.io");

    // b) Minted ids for generated slugs.
    assert_eq!(int_of(&r(&a, &[b"INCR", b"{su}seq"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"INCR", b"{su}seq"]).await), 2);

    // c) One-time preview token: GETDEL returns the target AND burns
    //    the key; a second read misses, TYPE says none.
    ok(
        &r(&a, &[b"SET", b"{su}tok:9f1", b"/promo/launch"]).await,
        "set token",
    );
    assert_eq!(rb(&a, &[b"GETDEL", b"{su}tok:9f1"]).await, "/promo/launch");
    assert_eq!(rt(&a, &[b"GET", b"{su}tok:9f1"]).await, "$-1");
    assert_eq!(rt(&a, &[b"TYPE", b"{su}tok:9f1"]).await, "+none");
    assert_eq!(rt(&a, &[b"GETDEL", b"{su}tok:gone"]).await, "$-1");

    // d) Redirect hotspot: SETEX caches for 60s, TTL is live, PERSIST
    //    pins it forever (-1).
    ok(
        &r(&a, &[b"SETEX", b"{su}hot:docs", b"60", b"/docs/latest"]).await,
        "setex",
    );
    let ttl = int_of(&r(&a, &[b"TTL", b"{su}hot:docs"]).await);
    assert!((1..=60).contains(&ttl), "live ttl, got {ttl}");
    assert_eq!(int_of(&r(&a, &[b"PERSIST", b"{su}hot:docs"]).await), 1);
    assert_eq!(
        int_of(&r(&a, &[b"TTL", b"{su}hot:docs"]).await),
        -1,
        "persisted"
    );

    // e) Domain whitelist patching: SETRANGE overwrites in place and
    //    answers the new length; GETRANGE clamps at both ends; a
    //    missing key slices to the empty bulk.
    ok(
        &r(&a, &[b"SET", b"{su}whitelist", b"acme.io, beta.dev"]).await,
        "set whitelist",
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETRANGE", b"{su}whitelist", b"0", b"acme.eu"]).await),
        17
    );
    assert_eq!(
        rb(&a, &[b"GET", b"{su}whitelist"]).await,
        "acme.eu, beta.dev"
    );
    assert_eq!(
        rb(&a, &[b"GETRANGE", b"{su}whitelist", b"0", b"6"]).await,
        "acme.eu"
    );
    assert_eq!(
        rb(&a, &[b"GETRANGE", b"{su}whitelist", b"-8", b"-1"]).await,
        "beta.dev"
    );
    assert_eq!(
        rb(&a, &[b"GETRANGE", b"{su}whitelist", b"99", b"120"]).await,
        "",
        "clamped past the end"
    );
    assert_eq!(rb(&a, &[b"GETRANGE", b"{su}missing", b"0", b"3"]).await, "");

    // f) A hash squatting the name must refuse string reads.
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"{su}kv:oops", b"f", b"v"]).await),
        1
    );
    assert!(rt(&a, &[b"GET", b"{su}kv:oops"])
        .await
        .starts_with("-WRONGTYPE"));

    // g) MySQL mirrors the registry.
    ddl(
        &mut c,
        "CREATE TABLE slugs (slug VARCHAR(32) PRIMARY KEY, target VARCHAR(128) NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO slugs (slug, target) VALUES ('home', 'https://acme.io')",
    )
    .await;
    assert_eq!(
        one(&mut c, "SELECT target FROM slugs WHERE slug = 'home'").await,
        s("https://acme.io")
    );
}
