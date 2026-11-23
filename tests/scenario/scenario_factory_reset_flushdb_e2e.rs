//! Multi-store scenario #101 -- a factory reset on the RESP plane:
//! FLUSHDB, the command registry's last true zero-scenario face. A
//! populated world (string + hash + list + zset + a lite stream with
//! history and a consumer group) gets the wipe: junk args are a
//! syntax error while ASYNC/SYNC pass through, then FLUSHDB answers
//! +OK and EVERY family reads empty -- DBSIZE :0, RANDOMKEY nil, the
//! stream's group memory died with the wipe (a rebuilt stream's
//! XINFO GROUPS answers a bare *0, matching the #91 reopen contract).
//! One REAL node, RESP only.

use crate::scenario_common;
use scenario_common::{front_world, int_of, r, rb, FrontSpec};

#[tokio::test]
async fn factory_reset_wipes_every_family_and_stream_groups() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("reset101", &FrontSpec::default()).await;

    // a) Populate every family: string, hash (root + field), list,
    //    zset, and a lite stream with one entry + a consumer group.
    let seeded: [(&[&[u8]], &str); 4] = [
        (&[b"SET", b"cfg:app", b"v1"], "+OK"),
        (
            &[b"HSET", b"cfg:props", b"region", b"eu", b"maxconn", b"512"],
            ":2",
        ),
        (&[b"LPUSH", b"queue:jobs", b"j1", b"j2", b"j3"], ":3"),
        (
            &[b"ZADD", b"rank:players", b"100", b"alice", b"85", b"bob"],
            ":2",
        ),
    ];
    for (args, want) in seeded {
        let got = r(&a, args).await;
        assert!(got.starts_with(want.as_bytes()), "{:?}: {:?}", args[0], got);
    }
    let xadd = r(&a, &[b"XADD", b"events/q0", b"*", b"kind", b"order"]).await;
    // full-name XADD answers the plain bulk id (bare parents get *2).
    assert!(xadd.starts_with(b"$"), "xadd events: {xadd:?}");
    let grp = r(
        &a,
        &[b"XGROUP", b"CREATE", b"events/q0", b"auditors", b"0-0"],
    )
    .await;
    assert!(grp.starts_with(b"+OK"), "xgroup create: {grp:?}");

    // b) The census before the wipe: five roots, a random key exists.
    assert!(int_of(&r(&a, &[b"DBSIZE"]).await) >= 5, "five family roots");
    let rand_key = rb(&a, &[b"RANDOMKEY"]).await;
    assert!(!rand_key.is_empty(), "randomkey hit: {rand_key:?}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"events/q0"]).await), 1);

    // c) Argument gate: junk is a syntax error (the wipe has NOT run).
    let junk = r(&a, &[b"FLUSHDB", b"FOO"]).await;
    assert!(junk.starts_with(b"-ERR syntax error"), "junk arg: {junk:?}");
    assert!(int_of(&r(&a, &[b"DBSIZE"]).await) >= 5, "still populated");

    // d) The wipe itself.
    let wipe = r(&a, &[b"FLUSHDB"]).await;
    assert!(wipe.starts_with(b"+OK"), "flushdb: {wipe:?}");

    // e) Every family reads empty: count, string, hash, list, zset,
    //    the stream root, and RANDOMKEY's nil.
    assert_eq!(int_of(&r(&a, &[b"DBSIZE"]).await), 0, "dbsize 0");
    assert!(
        r(&a, &[b"GET", b"cfg:app"]).await.starts_with(b"$-1"),
        "string gone"
    );
    assert!(
        r(&a, &[b"HGETALL", b"cfg:props"]).await.starts_with(b"*0"),
        "hash empty"
    );
    assert!(
        r(&a, &[b"LRANGE", b"queue:jobs", b"0", b"-1"])
            .await
            .starts_with(b"*0"),
        "list empty"
    );
    assert!(
        r(&a, &[b"ZRANGE", b"rank:players", b"0", b"-1"])
            .await
            .starts_with(b"*0"),
        "zset empty"
    );
    assert_eq!(
        int_of(&r(&a, &[b"EXISTS", b"events/q0"]).await),
        0,
        "stream root gone"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"events/q0"]).await),
        0,
        "stream empty"
    );
    assert_eq!(rb(&a, &[b"RANDOMKEY"]).await, "", "randomkey nil");

    // f) Rebuild path: a fresh XADD works and lands on an EMPTY
    //    family -- the wiped group did not survive (XINFO GROUPS is a
    //    bare *0, the #91 reopen contract).
    let rebuild = r(&a, &[b"XADD", b"events/q0", b"*", b"kind", b"reboot"]).await;
    assert!(rebuild.starts_with(b"$"), "xadd after wipe: {rebuild:?}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"events/q0"]).await), 1);
    let groups = r(&a, &[b"XINFO", b"GROUPS", b"events/q0"]).await;
    assert!(
        String::from_utf8_lossy(&groups).trim_end().ends_with("*0"),
        "groups reclaimed: {groups:?}"
    );

    // g) ASYNC/SYNC spellings pass through (idempotent on the now
    //    one-root world they wipe again).
    for mode in [&b"ASYNC"[..], &b"SYNC"[..]] {
        let got = r(&a, &[b"FLUSHDB", mode]).await;
        assert!(got.starts_with(b"+OK"), "flushdb {mode:?}: {got:?}");
    }
    assert_eq!(int_of(&r(&a, &[b"DBSIZE"]).await), 0, "empty again");
}
