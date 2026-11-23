//! Multi-store scenario #46 -- a DNS zone file served from RESP
//! strings: SETEX leases short-lived records, APPEND grows the TXT
//! chain, GETRANGE/SETRANGE slice and patch labels, GETSET rotates the
//! SOA serial, MSET/MGET batch-publish a label family, INCRBYFLOAT
//! weights the SRV records, and MySQL holds the zone registry the
//! serials reconcile against. One REAL node, RESP + MySQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rb, rt, FrontSpec};

#[tokio::test]
async fn dns_zone_strings_slice_patch_and_rotate() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("dns46", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) A-records with a short lease; TTL counts down from 60.
    scenario_common::ok(
        &r(&a, &[b"SETEX", b"{dn}www", b"60", b"203.0.113.10"]).await,
        "selex www",
    );
    let ttl = int_of(&r(&a, &[b"TTL", b"{dn}www"]).await);
    assert!(ttl == 59 || ttl == 60, "ttl set: {ttl}");
    assert_eq!(rb(&a, &[b"GET", b"{dn}www"]).await, "203.0.113.10");

    // b) TXT chain: APPEND grows the record, STRLEN agrees, GETRANGE
    //    slices a window, SETRANGE patches in place (reply = new len).
    scenario_common::ok(&r(&a, &[b"SET", b"{dn}txt", b"v=spf1"]).await, "set txt");
    assert_eq!(int_of(&r(&a, &[b"APPEND", b"{dn}txt", b" -all"]).await), 11);
    assert_eq!(int_of(&r(&a, &[b"STRLEN", b"{dn}txt"]).await), 11);
    assert_eq!(
        rb(&a, &[b"GETRANGE", b"{dn}txt", b"0", b"5"]).await,
        "v=spf1"
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETRANGE", b"{dn}txt", b"0", b"V=DKIM"]).await),
        11
    );
    assert_eq!(
        rb(&a, &[b"GETRANGE", b"{dn}txt", b"0", b"5"]).await,
        "V=DKIM"
    );

    // c) SOA serial rotation: GETSET returns the old serial while
    //    installing the new one.
    scenario_common::ok(
        &r(&a, &[b"SET", b"{dn}soa", b"2026112101"]).await,
        "set soa",
    );
    assert_eq!(
        rb(&a, &[b"GETSET", b"{dn}soa", b"2026112102"]).await,
        "2026112101"
    );
    assert_eq!(rb(&a, &[b"GET", b"{dn}soa"]).await, "2026112102");

    // d) Batch publish a label family: MSET + MGET (same-slot tags).
    scenario_common::ok(
        &r(
            &a,
            &[
                b"MSET", b"{dn}a", b"1.2.3.4", b"{dn}b", b"5.6.7.8", b"{dn}c", b"9.9.9.9",
            ],
        )
        .await,
        "mset family",
    );
    let fam = scenario_common::rf(&a, &[b"MGET", b"{dn}a", b"{dn}zz", b"{dn}c"]).await;
    assert!(fam.contains("1.2.3.4") && fam.contains("9.9.9.9"), "{fam}");
    assert!(fam.contains("$-1"), "missing middle label is nil: {fam}");

    // e) SRV weights drift by fractions; INCRBYFLOAT keeps the
    //    shortest roundtrip.
    scenario_common::ok(&r(&a, &[b"SET", b"{dn}srv:1", b"10"]).await, "set srv");
    assert_eq!(
        rb(&a, &[b"INCRBYFLOAT", b"{dn}srv:1", b"0.5"]).await,
        "10.5"
    );
    assert_eq!(
        rb(&a, &[b"INCRBYFLOAT", b"{dn}srv:1", b"0.25"]).await,
        "10.75"
    );
    assert_eq!(rb(&a, &[b"INCRBYFLOAT", b"{dn}srv:1", b"0.25"]).await, "11");

    // f) Zone registry in MySQL: three published families, the SOA
    //    serial matches the rotated value.
    ddl(
        &mut c,
        "CREATE TABLE zones (id BIGINT PRIMARY KEY, zone VARCHAR(32) NOT NULL, \
         serial BIGINT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO zones (id, zone, serial) VALUES (1, 'example.net', 2026112102)",
    )
    .await;
    assert_eq!(
        ints(
            &mut c,
            "SELECT serial FROM zones WHERE zone = 'example.net'"
        )
        .await,
        vec![2026112102],
        "registry tracks the rotated SOA"
    );
    assert_eq!(
        int_of(&r(&a, &[b"EXISTS", b"{dn}www", b"{dn}txt"]).await),
        2
    );
    assert_eq!(
        rt(&a, &[b"TYPE", b"{dn}soa"]).await.trim_start_matches('+'),
        "string"
    );
}
