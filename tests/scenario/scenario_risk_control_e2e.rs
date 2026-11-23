//! Multi-store scenario #7 -- a risk-control gateway over five KV
//! shapes plus a SQL audit table: fixed-window rate limiting with
//! INCR+EXPIRE (string_e2e), a Set blacklist, a device-fingerprint
//! bloom sketch with SETBIT/GETBIT (bits_e2e), a sliding window in a
//! ZSET pruned by ZREMRANGEBYSCORE (zset_e2e), and every verdict
//! appended to SQL for the regulator's reconciliation.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rf, FrontSpec};

#[tokio::test]
async fn risk_control_windows_blacklist_bloom_and_audit() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("risk7", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE risk_audit (seq BIGINT PRIMARY KEY, ip VARCHAR(64), verdict VARCHAR(8), reason VARCHAR(32))",
    )
    .await;

    // a) Fixed window: 4 hits inside the minute window pass, the 5th is
    //    over the limit; a fresh window (new key) starts clean.
    for i in 1..=4 {
        let n = int_of(&r(&a, &[b"INCR", b"rate:api:1.2.3.4:win1"]).await);
        assert_eq!(n, i, "hit {i}");
    }
    assert_eq!(
        int_of(&r(&a, &[b"INCR", b"rate:api:1.2.3.4:win1"]).await),
        5,
        "over the limit"
    );
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", b"rate:api:1.2.3.4:win1", b"60"]).await),
        1
    );
    run(
        &mut c,
        "INSERT INTO risk_audit (seq, ip, verdict, reason) VALUES (1, '1.2.3.4', 'deny', 'rate')",
    )
    .await;
    assert_eq!(
        int_of(&r(&a, &[b"INCR", b"rate:api:1.2.3.4:win2"]).await),
        1,
        "next window clean"
    );
    let ttl = int_of(&r(&a, &[b"TTL", b"rate:api:1.2.3.4:win1"]).await);
    assert!((1..=60).contains(&ttl), "window ttl: {ttl}");

    // b) Blacklist: a Set of banned IPs; membership short-circuits.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"risk:blacklist", b"9.9.9.9", b"8.8.8.8"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SISMEMBER", b"risk:blacklist", b"9.9.9.9"]).await),
        1
    );
    run(&mut c, "INSERT INTO risk_audit (seq, ip, verdict, reason) VALUES (2, '9.9.9.9', 'deny', 'blacklist')").await;

    // c) Device fingerprints: one bit per (device bucket, id) pair in a
    //    64-slot sketch; a repeat device reads its bit back.
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"risk:fp:bucket7", b"9", b"1"]).await),
        0,
        "bit was 0"
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"risk:fp:bucket7", b"41", b"1"]).await),
        0,
        "bit was 0"
    );
    assert_eq!(
        int_of(&r(&a, &[b"GETBIT", b"risk:fp:bucket7", b"9"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"GETBIT", b"risk:fp:bucket7", b"10"]).await),
        0
    );
    assert_eq!(int_of(&r(&a, &[b"BITCOUNT", b"risk:fp:bucket7"]).await), 2);

    // d) Sliding window: hits scored by their own timestamp; pruning
    //    everything older than the 10s horizon leaves exactly the
    //    fresh hits.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"ZADD",
                    b"risk:slide:5.6.7.8",
                    b"1",
                    b"hit:1",
                    b"2",
                    b"hit:2",
                    b"3",
                    b"hit:3"
                ]
            )
            .await
        ),
        3
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"risk:slide:5.6.7.8", b"40", b"hit:0"]).await),
        1
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"ZREMRANGEBYSCORE", b"risk:slide:5.6.7.8", b"(10", b"+inf"]
            )
            .await
        ),
        1,
        "only hit:0 pruned"
    );
    let fresh = rf(
        &a,
        &[b"ZRANGE", b"risk:slide:5.6.7.8", b"0", b"-1", b"WITHSCORES"],
    )
    .await;
    assert!(
        fresh.contains("hit:1") && !fresh.contains("hit:0"),
        "{fresh}"
    );
    assert_eq!(int_of(&r(&a, &[b"ZCARD", b"risk:slide:5.6.7.8"]).await), 3);

    // e) Verdict flow: an allowed request (rate ok, not blacklisted,
    //    device known) lands in SQL as allow.
    run(
        &mut c,
        "INSERT INTO risk_audit (seq, ip, verdict, reason) VALUES (3, '5.6.7.8', 'allow', '-')",
    )
    .await;

    // f) Reconciliation: SQL sees every verdict; deny reasons split
    //    exactly as the KV layers decided them.
    let total = ints(&mut c, "SELECT COUNT(*) FROM risk_audit").await;
    assert_eq!(total, vec![3]);
    let denies = ints(
        &mut c,
        "SELECT COUNT(*) FROM risk_audit WHERE verdict = 'deny'",
    )
    .await;
    assert_eq!(denies, vec![2]);
    let reasons = ints(
        &mut c,
        "SELECT COUNT(*) FROM risk_audit WHERE reason IN ('rate', 'blacklist')",
    )
    .await;
    assert_eq!(reasons, vec![2], "each deny names its layer");
    // The blacklist Set survives as the deny-source of truth.
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"risk:blacklist"]).await), 2);
}
