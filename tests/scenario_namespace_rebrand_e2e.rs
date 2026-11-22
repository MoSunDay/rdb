//! Multi-store scenario #72 -- a namespace rebrand: RENAME moves the
//! payload AND the lease deadline to the new name (the old name is
//! gone, TTL = -2) and clobbers whatever squatted the destination,
//! RENAMENX refuses to overwrite (:0) and wins a free name (:1), a
//! missing source errors `-ERR no such key` without materializing the
//! destination, cross-slot pairs CROSSSLOT while a shared hash tag
//! dodges the gate, and the value's TYPE rides along. One REAL node,
//! RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, int_of, ok, r, rb, rt, FrontSpec};

#[tokio::test]
async fn rebrand_moves_ttl_type_and_gates_cross_slot() {
    let (_front, a) = front_world("nbr72", &FrontSpec::default()).await;

    // a) The rebrand: a leased key moves name AND deadline together.
    ok(&r(&a, &[b"SETEX", b"{nb}svc:old", b"500", b"v1"]).await, "lease the old name");
    assert_eq!(rt(&a, &[b"RENAME", b"{nb}svc:old", b"{nb}svc:new"]).await, "+OK");
    assert_eq!(rb(&a, &[b"GET", b"{nb}svc:new"]).await, "v1", "payload moved");
    assert!(int_of(&r(&a, &[b"TTL", b"{nb}svc:new"]).await) > 0, "ttl rides the rename");
    assert_eq!(int_of(&r(&a, &[b"TTL", b"{nb}svc:old"]).await), -2, "old name gone");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{nb}svc:old"]).await), 0);

    // b) Destination overwrite: RENAME clobbers a squatter.
    ok(&r(&a, &[b"SET", b"{nb}a", b"1"]).await, "set a");
    ok(&r(&a, &[b"SET", b"{nb}b", b"2"]).await, "set b");
    assert_eq!(rt(&a, &[b"RENAME", b"{nb}a", b"{nb}b"]).await, "+OK");
    assert_eq!(rb(&a, &[b"GET", b"{nb}b"]).await, "1", "dst overwritten");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{nb}a"]).await), 0);

    // c) RENAMENX: :0 when the destination exists, :1 when free.
    ok(&r(&a, &[b"SET", b"{nb}c", b"3"]).await, "set c");
    assert_eq!(int_of(&r(&a, &[b"RENAMENX", b"{nb}c", b"{nb}b"]).await), 0, "nx refuses");
    assert_eq!(rb(&a, &[b"GET", b"{nb}b"]).await, "1", "b untouched");
    assert_eq!(rb(&a, &[b"GET", b"{nb}c"]).await, "3", "c survives the refusal");
    assert_eq!(int_of(&r(&a, &[b"RENAMENX", b"{nb}c", b"{nb}d"]).await), 1);
    assert_eq!(rb(&a, &[b"GET", b"{nb}d"]).await, "3");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{nb}c"]).await), 0, "moved for real");

    // d) Missing source: the destination never materializes.
    assert_eq!(rt(&a, &[b"RENAME", b"{nb}ghost", b"{nb}z"]).await, "-ERR no such key");
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{nb}z"]).await), 0);

    // e) Cross-slot pairs refuse; the tagged pair passed in (a).
    ok(&r(&a, &[b"SET", b"{nb}x", b"v"]).await, "set x");
    assert_eq!(
        rt(&a, &[b"RENAME", b"{nb}x", b"{zz}y"]).await,
        "-ERR CROSSSLOT Keys in request don't hash to the same slot"
    );
    assert_eq!(rb(&a, &[b"GET", b"{nb}x"]).await, "v", "refused = untouched");

    // f) TYPE rides along: a list stays a list across the rename.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{nb}lst", b"one"]).await), 1);
    assert_eq!(rt(&a, &[b"RENAME", b"{nb}lst", b"{nb}seq"]).await, "+OK");
    assert_eq!(rt(&a, &[b"TYPE", b"{nb}seq"]).await, "+list");
    assert_eq!(rb(&a, &[b"LPOP", b"{nb}seq"]).await, "one");
}
