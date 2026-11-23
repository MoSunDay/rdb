//! Multi-store scenario #84 -- warranty deadlines behind EXPIRE's four
//! fences: NX arms a deadline only when none exists (second NX is a no-
//! op :0), LT shrinks an armed deadline but never extends it, GT
//! extends but never shrinks, XX touches only already-armed keys (a
//! bare XX on a live-but-undeadlined key is :0); every outcome is the
//! honest :1/:0 with the TTL moving (or holding) exactly as fenced, an
//! unknown flag refuses with the verbatim option list, and a fence on a
//! missing key stays :0. One REAL node, RESP only.

use crate::scenario_common;
use scenario_common::{front_world, int_of, r, rt, FrontSpec};

#[tokio::test]
async fn warranty_fences_only_move_deadlines_their_way() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("tf84", &FrontSpec::default()).await;
    let ttl = |key: &[u8]| {
        let a = a.clone();
        let k = key.to_vec();
        async move { int_of(&r(&a, &[b"TTL", &k]).await) }
    };

    // a) NX arms the warranty: :1 once, :0 when one already runs.
    let w = b"{tf84}warranty";
    assert_eq!(rt(&a, &[b"SET", w, b"active"]).await, "+OK");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", w, b"100", b"NX"]).await),
        1,
        "NX arms"
    );
    let armed = ttl(w).await;
    assert!((95..=100).contains(&armed), "armed window: {armed}");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", w, b"200", b"NX"]).await),
        0,
        "NX again"
    );
    assert!(
        (95..=105).contains(&ttl(w).await),
        "second NX did not extend"
    );

    // b) LT shrinks (:1) but never extends (:0, TTL holds).
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", w, b"50", b"LT"]).await),
        1,
        "LT shrinks"
    );
    let shrunk = ttl(w).await;
    assert!((45..=55).contains(&shrunk), "shrunk window: {shrunk}");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", w, b"500", b"LT"]).await),
        0,
        "LT never extends"
    );
    assert!((40..=55).contains(&ttl(w).await), "window held");

    // c) GT extends (:1) but never shrinks (:0).
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", w, b"400", b"GT"]).await),
        1,
        "GT extends"
    );
    assert!(ttl(w).await > 300, "extended window");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", w, b"10", b"GT"]).await),
        0,
        "GT never shrinks"
    );
    assert!(ttl(w).await > 300, "extended window held");

    // d) XX touches only armed keys: bare-then-arm-then-extend.
    let p = b"{tf84}plain";
    assert_eq!(rt(&a, &[b"SET", p, b"bare"]).await, "+OK");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", p, b"100", b"XX"]).await),
        0,
        "XX on bare"
    );
    assert_eq!(int_of(&r(&a, &[b"EXPIRE", p, b"100"]).await), 1, "arm it");
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", p, b"300", b"XX"]).await),
        1,
        "XX extends armed"
    );

    // e) A fence on a missing key stays :0; an unknown flag refuses
    //    with the verbatim option list.
    assert_eq!(
        int_of(&r(&a, &[b"EXPIRE", b"{tf84}ghost", b"100", b"NX"]).await),
        0
    );
    assert_eq!(
        rt(&a, &[b"EXPIRE", w, b"100", b"ONLY"]).await,
        "-ERR Unsupported option: supported options are NX, XX, GT and LT"
    );
}
