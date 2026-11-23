//! Multi-store scenario #42 -- a fraud ring hunt across shared
//! devices: per-user device sets meet on SINTERCARD (shared gear),
//! SDIFF isolates unique devices, SUNIONSTORE rolls the ring-wide
//! footprint, SETBIT+BITOP AND cross-checks the fingerprint bytes,
//! and every finding lands in the MySQL case table. One REAL node,
//! RESP + MySQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, r, rf, FrontSpec};

#[tokio::test]
async fn shared_devices_surface_a_fraud_ring() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("fraud42", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Device sets (hash-tagged so set algebra stays same-slot):
    //    u1/u2 share d2, u2/u3 share d3 -- the ring is u1-u2-u3.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{ring}u1", b"d1", b"d2"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{ring}u2", b"d2", b"d3"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{ring}u3", b"d3", b"d4"]).await),
        2
    );
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{ring}u4", b"d9"]).await), 1);

    // b) Pairwise overlap: SINTERCARD (numkeys prefix).
    assert_eq!(
        int_of(&r(&a, &[b"SINTERCARD", b"2", b"{ring}u1", b"{ring}u2"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"SINTERCARD", b"2", b"{ring}u1", b"{ring}u4"]).await),
        0
    );

    // c) Unique gear: SDIFF u1 u2 leaves only d1.
    let uniq = rf(&a, &[b"SDIFF", b"{ring}u1", b"{ring}u2"]).await;
    assert_eq!(bulk_at(uniq.as_bytes(), 0), "d1", "{uniq}");

    // d) Ring-wide footprint: SUNIONSTORE folds all three into one set.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"SUNIONSTORE",
                    b"{ring}all",
                    b"{ring}u1",
                    b"{ring}u2",
                    b"{ring}u3"
                ]
            )
            .await
        ),
        4,
        "d1..d4 deduped"
    );
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"{ring}all"]).await), 4);
    assert_eq!(
        int_of(&r(&a, &[b"SISMEMBER", b"{ring}all", b"d3"]).await),
        1
    );

    // e) Fingerprint bits: one bit per device slot; BITOP answers the
    //    RESULT byte length (one byte here), BITCOUNT reads the
    //    merged popcounts: AND keeps the shared bit, OR spans all
    //    three devices.
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"{ring}f1", b"1", b"1"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"{ring}f1", b"2", b"1"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"{ring}f2", b"2", b"1"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"SETBIT", b"{ring}f2", b"3", b"1"]).await),
        0
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"BITOP", b"AND", b"{ring}fand", b"{ring}f1", b"{ring}f2"]
            )
            .await
        ),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"BITCOUNT", b"{ring}fand"]).await),
        1,
        "only bit 2 is common"
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"BITOP", b"OR", b"{ring}for", b"{ring}f1", b"{ring}f2"]
            )
            .await
        ),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"BITCOUNT", b"{ring}for"]).await),
        3,
        "bits 1,2,3 across either user"
    );

    // f) Risk scores climb per finding.
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"risk:score", b"u1", b"10"]).await),
        10
    );
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"risk:score", b"u2", b"20"]).await),
        20
    );
    assert_eq!(
        int_of(&r(&a, &[b"HINCRBY", b"risk:score", b"u2", b"5"]).await),
        25
    );

    // g) Case table: one row per implicated pair.
    ddl(
        &mut c,
        "CREATE TABLE fraud_cases (id BIGINT PRIMARY KEY, pair VARCHAR(16) NOT NULL, \
         shared_device VARCHAR(16) NOT NULL, risk INT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO fraud_cases (id, pair, shared_device, risk) \
                 VALUES (1, 'u1-u2', 'd2', 35), (2, 'u2-u3', 'd3', 45)",
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT SUM(risk) FROM fraud_cases").await,
        vec![80]
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM fraud_cases WHERE shared_device LIKE 'd%'"
        )
        .await,
        vec![2]
    );
    // The honest user never entered the ledger.
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM fraud_cases WHERE pair LIKE '%u4%'"
        )
        .await,
        vec![0]
    );
    assert_eq!(
        int_of(&r(&a, &[b"SISMEMBER", b"{ring}all", b"d9"]).await),
        0
    );
}
