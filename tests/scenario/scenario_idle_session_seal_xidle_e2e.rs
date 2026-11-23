//! Multi-store scenario #91 -- idle support-session streams sealed on
//! a timer: XIDLE queries the CONFIGURED idle seconds (-1 none, -2 no
//! such stream), sets a 1s idle TTL that reclaims the WHOLE stream
//! family when it fires (entries + group + PEL vanish; a fresh XADD
//! after the seal sees no leftover group state), and 0 clears the
//! timer again -- the XIDLE surface no earlier scenario touched. The
//! expiry wait polls the reply instead of sleeping a fixed span (the
//! active loop samples every 100ms and reads purge lazily too).

use crate::scenario_common;
use scenario_common::{front_world, int_of, r, rf, until_contains, FrontSpec};

#[tokio::test]
async fn idle_sessions_seal_whole_families_on_the_idle_timer() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("idle91", &FrontSpec::default()).await;

    // a) Two counselor session streams with history; one gets a
    //    group so the family has entry/group/pending shapes to
    //    reclaim.
    for s in ["counsel/alice", "counsel/bob"] {
        for i in 1..=2 {
            assert!(
                r(
                    &a,
                    &[
                        b"XADD",
                        s.as_bytes(),
                        b"*",
                        b"msg",
                        format!("m{i}").as_bytes()
                    ]
                )
                .await
                .starts_with(b"$"),
                "xadd {s}"
            );
        }
    }
    ok_xadd_group(&a, b"counsel/alice").await;
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"counsel/alice"]).await), 2);

    // b) Query matrix: -1 = live but no idle configured, -2 = stream
    //    missing (distinct from an empty-but-live stream).
    assert_eq!(
        int_of(&r(&a, &[b"XIDLE", b"counsel/alice"]).await),
        -1,
        "no timer yet"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XIDLE", b"counsel/nobody"]).await),
        -2,
        "missing stream"
    );

    // c) Arming reports back the configured seconds; 0 clears it.
    assert!(r(&a, &[b"XIDLE", b"counsel/bob", b"30"])
        .await
        .starts_with(b"+OK"));
    assert_eq!(
        int_of(&r(&a, &[b"XIDLE", b"counsel/bob"]).await),
        30,
        "configured secs"
    );
    assert!(r(&a, &[b"XIDLE", b"counsel/bob", b"0"])
        .await
        .starts_with(b"+OK"));
    assert_eq!(
        int_of(&r(&a, &[b"XIDLE", b"counsel/bob"]).await),
        -1,
        "0 clears"
    );

    // d) Setting on a missing stream refuses (no auto-create) and the
    //    bogus-seconds form is rejected without arming anything.
    assert!(
        r(&a, &[b"XIDLE", b"counsel/nobody", b"5"])
            .await
            .starts_with(b"-ERR no such key"),
        "no auto-create"
    );
    assert!(
        r(&a, &[b"XIDLE", b"counsel/bob", b"abc"])
            .await
            .starts_with(b"-ERR invalid idle seconds"),
        "bogus seconds"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XIDLE", b"counsel/bob"]).await),
        -1,
        "still clear"
    );

    // e) The seal: arm 1s on the grouped stream, let the idle timer
    //    fire (active loop samples at 100ms; the query read itself
    //    lazily purges too), then the WHOLE family is gone.
    assert!(r(&a, &[b"XIDLE", b"counsel/alice", b"1"])
        .await
        .starts_with(b"+OK"));
    until_contains(
        &a,
        &[b"XIDLE", b"counsel/alice"],
        "-2",
        "idle seal fires",
        10,
    )
    .await;
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"counsel/alice"]).await),
        0,
        "entries reclaimed"
    );
    assert_eq!(
        int_of(&r(&a, &[b"EXISTS", b"counsel/alice"]).await),
        0,
        "root gone"
    );

    // f) Family scope proof: a fresh XADD after the seal rebuilds an
    //    EMPTY stream -- the old group did not survive the reclaim
    //    (XINFO GROUPS answers a bare *0, not the sealed group).
    assert!(
        r(&a, &[b"XADD", b"counsel/alice", b"*", b"msg", b"reopened"])
            .await
            .starts_with(b"$")
    );
    let groups = rf(&a, &[b"XINFO", b"GROUPS", b"counsel/alice"]).await;
    assert!(
        groups.trim_end().ends_with("*0"),
        "group reclaimed with family: {groups:?}"
    );
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"counsel/alice"]).await), 1);
    assert_eq!(
        int_of(&r(&a, &[b"XIDLE", b"counsel/alice"]).await),
        -1,
        "seal consumed"
    );

    // g) The sibling stream kept its history (the seal is per stream,
    //    not per slot window).
    assert_eq!(
        int_of(&r(&a, &[b"XLEN", b"counsel/bob"]).await),
        2,
        "sibling untouched"
    );
}

/// XGROUP CREATE at 0-0 (fresh stream family, no earlier group).
async fn ok_xadd_group(a: &str, stream: &[u8]) {
    let reply = r(a, &[b"XGROUP", b"CREATE", stream, b"counselors", b"0-0"]).await;
    assert!(reply.starts_with(b"+OK"), "xgroup create: {reply:?}");
}
