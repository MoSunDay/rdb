//! Multi-store scenario #71 -- a lease TTL patrol: SETEX/PSETEX mint
//! deadline-backed keys whose TTL/PTTL echo the live window, EXPIRE /
//! PEXPIRE re-arm running leases, EXPIREAT/PEXPIREAT pin absolute
//! unix-second/millisecond deadlines, TTL rounds the remaining
//! milliseconds HALF-UP ((ms+500)/1000: a 1.6s lease reads 2, not the
//! floored 1), PERSIST clears to the eternal -1, and zero/negative
//! mint arguments refuse verbatim. One REAL node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, int_of, ok, r, rt, FrontSpec};

#[tokio::test]
async fn lease_grid_mints_rearms_pins_and_persists() {
    let (_front, a) = front_world("lease71", &FrontSpec::default()).await;

    // a) SETEX mints a second-grained lease: TTL sits inside the
    //    window, PTTL carries the fine remainder.
    ok(&r(&a, &[b"SETEX", b"{lz}a", b"90", b"v1"]).await, "setex");
    let ttl = int_of(&r(&a, &[b"TTL", b"{lz}a"]).await);
    assert!((1..=90).contains(&ttl), "setex ttl window, got {ttl}");
    let pttl = int_of(&r(&a, &[b"PTTL", b"{lz}a"]).await);
    assert!((80_000..=90_000).contains(&pttl), "setex pttl, got {pttl}");

    // b) PSETEX: the millisecond twin.
    ok(&r(&a, &[b"PSETEX", b"{lz}b", b"5000", b"v2"]).await, "psetex");
    let pms = int_of(&r(&a, &[b"PTTL", b"{lz}b"]).await);
    assert!((4_000..=5_000).contains(&pms), "psetex pttl, got {pms}");
    assert!(
        (1..=5).contains(&int_of(&r(&a, &[b"TTL", b"{lz}b"]).await)),
        "psetex ttl seconds"
    );

    // c) Half-UP rounding: a 1.6s lease reads TTL 2 (floored = 1).
    ok(&r(&a, &[b"SET", b"{lz}c", b"v3"]).await, "set c");
    assert_eq!(int_of(&r(&a, &[b"PEXPIRE", b"{lz}c", b"1600"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"TTL", b"{lz}c"]).await), 2, "half-up, not floored");
    let fine = int_of(&r(&a, &[b"PTTL", b"{lz}c"]).await);
    assert!((1_500..=1_600).contains(&fine), "pttl remainder, got {fine}");

    // d) EXPIRE re-arms a plain key; a missing key refuses (:0).
    ok(&r(&a, &[b"SET", b"{lz}d", b"v4"]).await, "set d");
    assert_eq!(int_of(&r(&a, &[b"EXPIRE", b"{lz}d", b"60"]).await), 1);
    assert!((1..=60).contains(&int_of(&r(&a, &[b"TTL", b"{lz}d"]).await)));
    assert_eq!(int_of(&r(&a, &[b"EXPIRE", b"{lz}ghost", b"60"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"TTL", b"{lz}ghost"]).await), -2);

    // e) Absolute deadlines: EXPIREAT unix seconds, PEXPIREAT unix ms.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    ok(&r(&a, &[b"SET", b"{lz}e", b"v5"]).await, "set e");
    let at_s = now.as_secs() + 120;
    assert_eq!(
        int_of(&r(&a, &[b"EXPIREAT", b"{lz}e", at_s.to_string().as_bytes()]).await),
        1
    );
    assert!((1..=120).contains(&int_of(&r(&a, &[b"TTL", b"{lz}e"]).await)));
    ok(&r(&a, &[b"SET", b"{lz}f", b"v6"]).await, "set f");
    let at_ms = now.as_millis() + 7_000;
    assert_eq!(
        int_of(&r(&a, &[b"PEXPIREAT", b"{lz}f", at_ms.to_string().as_bytes()]).await),
        1
    );
    let pf = int_of(&r(&a, &[b"PTTL", b"{lz}f"]).await);
    assert!((6_000..=7_000).contains(&pf), "pexpireat pttl, got {pf}");

    // f) PERSIST clears the lease: -1 forever; the second call :0.
    assert_eq!(int_of(&r(&a, &[b"PERSIST", b"{lz}d"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"TTL", b"{lz}d"]).await), -1, "persisted");
    assert_eq!(int_of(&r(&a, &[b"PERSIST", b"{lz}d"]).await), 0, "nothing left");

    // g) Invalid lease lengths refuse verbatim at mint time.
    assert_eq!(
        rt(&a, &[b"SETEX", b"{lz}x", b"0", b"v"]).await,
        "-ERR invalid expire time in 'setex' command"
    );
    assert_eq!(
        rt(&a, &[b"PSETEX", b"{lz}x", b"0", b"v"]).await,
        "-ERR invalid expire time in 'psetex' command"
    );
    assert_eq!(
        rt(&a, &[b"SETEX", b"{lz}x", b"-5", b"v"]).await,
        "-ERR invalid expire time in 'setex' command"
    );
    assert_eq!(
        rt(&a, &[b"PSETEX", b"{lz}x", b"-1", b"v"]).await,
        "-ERR invalid expire time in 'psetex' command"
    );
}
