//! Multi-store scenario #83 -- a login-session store through the SET
//! flag matrix: GET-mode writes return the previous value (nil for a
//! brand-new session), NX/XX vetoes still honor GET (the veto replies
//! the old value; a plain veto is a nil bulk), NX+GET on a fresh key
//! writes without error, KEEPTTL preserves a live TTL across a value
//! rewrite while a bare SET clears it, and the two incompatible flag
//! pairs (NX with XX, KEEPTTL with EX) fail as syntax errors. One REAL
//! node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{front_world, r, rt, FrontSpec};

#[tokio::test]
async fn session_flags_matrix_rounds_the_login_lifecycle() {
    let (_front, a) = front_world("sf83", &FrontSpec::default()).await;
    let k = b"{sf83}session";

    // a) GET-mode write on a brand-new session: nil old value, then the
    //    value is there; the next GET-mode write returns the OLD one.
    assert!(r(&a, &[b"SET", k, b"v1", b"GET"]).await.starts_with(b"$-1"));
    assert_eq!(rt(&a, &[b"GET", k]).await.trim_end(), "$2\r\nv1");
    assert!(r(&a, &[b"SET", k, b"v2", b"GET"]).await.starts_with(b"$2\r\nv1"));

    // b) XX passes on a present key and GET still returns the old value.
    assert!(r(&a, &[b"SET", k, b"v3", b"XX", b"GET"]).await.starts_with(b"$2\r\nv2"));
    assert_eq!(rt(&a, &[b"GET", k]).await.trim_end(), "$2\r\nv3");

    // c) NX+GET on a FRESH key is no error: nil old value, write lands.
    let fresh = b"{sf83}fresh";
    assert!(r(&a, &[b"SET", fresh, b"go", b"NX", b"GET"]).await.starts_with(b"$-1"));
    assert_eq!(rt(&a, &[b"GET", fresh]).await.trim_end(), "$2\r\ngo");

    // d) NX veto on the present key: plain nil bulk; with GET the veto
    //    still reports the old value; XX veto on an absent key is nil
    //    and never writes.
    assert!(r(&a, &[b"SET", fresh, b"x", b"NX"]).await.starts_with(b"$-1"));
    assert!(r(&a, &[b"SET", fresh, b"x", b"NX", b"GET"]).await.starts_with(b"$2\r\ngo"));
    assert_eq!(rt(&a, &[b"GET", fresh]).await.trim_end(), "$2\r\ngo");
    let absent = b"{sf83}absent";
    assert!(r(&a, &[b"SET", absent, b"x", b"XX"]).await.starts_with(b"$-1"));
    assert!(r(&a, &[b"GET", absent]).await.starts_with(b"$-1"));

    // e) KEEPTTL: the TTL survives the rewrite (still counting down),
    //    a bare SET clears it to -1.
    let ttl_key = b"{sf83}lease";
    assert_eq!(rt(&a, &[b"SET", ttl_key, b"held", b"EX", b"100"]).await, "+OK");
    assert_eq!(rt(&a, &[b"SET", ttl_key, b"renewed", b"KEEPTTL"]).await, "+OK");
    let kept = scenario_common::int_of(&r(&a, &[b"TTL", ttl_key]).await);
    assert!((90..=100).contains(&kept), "KEEPTTL keeps the clock: {kept}");
    assert_eq!(rt(&a, &[b"SET", ttl_key, b"plain"]).await, "+OK");
    assert_eq!(scenario_common::int_of(&r(&a, &[b"TTL", ttl_key]).await), -1);

    // f) Incompatible flag pairs are syntax errors, verbatim.
    assert_eq!(
        rt(&a, &[b"SET", k, b"x", b"NX", b"XX"]).await,
        "-ERR syntax error"
    );
    assert_eq!(
        rt(&a, &[b"SET", k, b"x", b"KEEPTTL", b"EX", b"100"]).await,
        "-ERR syntax error"
    );
}
