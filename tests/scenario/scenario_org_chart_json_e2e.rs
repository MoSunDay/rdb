//! Multi-store scenario #65 -- the org chart file: JSON.SET roots an
//! object, JSON.OBJLEN/OBJKEYS introspect it, a scalar side-array
//! exercises ARRINDEX windows, ARRTRIM, ARRPOP and ARRAPPEND,
//! STRAPPEND/STRLEN grow the org name, NUMINCRBY moves headcount in
//! compact form and JSON.DEL retires a subtree; the department
//! directory rides a hash read back flat with HGETALL/HKEYS/HVALS and
//! sized with HSTRLEN. MySQL keeps the headcount rollup. One REAL
//! node, RESP + MySQL.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, one, run};
use scenario_common::{bulk_at, front_world, int_of, ok, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn chart_introspection_trim_and_directory() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("org65", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Root the chart: OBJLEN counts the root keys, OBJKEYS lists
    //    them, and a missing path answers a nil bulk.
    ok(&r(&a, &[b"JSON.SET", b"{oc}chart", b"$",
        br#"{"org":"acme","headcount":3,"meta":{"hq":"sjc"},"teams":[{"name":"core","size":5},{"name":"web","size":3}]}"#.as_slice()]).await, "json.set");
    assert_eq!(
        int_of(&r(&a, &[b"JSON.OBJLEN", b"{oc}chart", b"$"]).await),
        4
    );
    let keys = rf(&a, &[b"JSON.OBJKEYS", b"{oc}chart", b"$"]).await;
    for (i, want) in ["org", "headcount", "meta", "teams"].iter().enumerate() {
        assert_eq!(&bulk_at(keys.as_bytes(), i), want, "objkeys {i}: {keys}");
    }
    assert_eq!(
        int_of(&r(&a, &[b"JSON.OBJLEN", b"{oc}chart", b"$.meta"]).await),
        1
    );
    assert_eq!(
        rt(&a, &[b"JSON.OBJKEYS", b"{oc}chart", b"$.nope"]).await,
        "$-1"
    );

    // b) Headcount moves in compact form; the org name grows.
    assert_eq!(
        rb(&a, &[b"JSON.NUMINCRBY", b"{oc}chart", b"$.headcount", b"2"]).await,
        "5"
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"JSON.STRAPPEND",
                    b"{oc}chart",
                    b"$.org",
                    br#""-labs""#.as_slice()
                ]
            )
            .await
        ),
        9
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.STRLEN", b"{oc}chart", b"$.org"]).await),
        9
    );
    assert!(
        rt(&a, &[b"JSON.STRLEN", b"{oc}chart", b"$.headcount"])
            .await
            .starts_with("-ERR"),
        "strlen only eats strings"
    );

    // c) A scalar side-array: ARRAPPEND lands a new grade, ARRINDEX
    //    finds it (and honors the start/stop window), ARRTRIM keeps
    //    the live band and ARRPOP peeks from either end.
    ok(
        &r(&a, &[b"JSON.SET", b"{oc}grades", b"$", b"[7,8,9]"]).await,
        "grades",
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.ARRAPPEND", b"{oc}grades", b"$", b"10"]).await),
        4
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.ARRINDEX", b"{oc}grades", b"$", b"9"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.ARRINDEX", b"{oc}grades", b"$", b"9", b"3"]).await),
        -1,
        "window starts past it"
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"JSON.ARRINDEX", b"{oc}grades", b"$", b"9", b"0", b"2"]
            )
            .await
        ),
        -1,
        "stop is EXCLUSIVE (rdb deviation)"
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"JSON.ARRINDEX", b"{oc}grades", b"$", b"9", b"0", b"3"]
            )
            .await
        ),
        2,
        "[0,3) holds it"
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.ARRTRIM", b"{oc}grades", b"$", b"1", b"2"]).await),
        2
    );
    assert_eq!(
        rb(&a, &[b"JSON.ARRPOP", b"{oc}grades", b"$", b"-1"]).await,
        "9"
    );
    assert_eq!(rb(&a, &[b"JSON.ARRPOP", b"{oc}grades"]).await, "8");
    assert_eq!(rt(&a, &[b"JSON.ARRPOP", b"{oc}gradesgone"]).await, "$-1");

    // d) JSON.DEL retires the meta subtree once (:1) and misses (:0).
    assert_eq!(
        int_of(&r(&a, &[b"JSON.DEL", b"{oc}chart", b"$.meta"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.DEL", b"{oc}chart", b"$.meta"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"JSON.OBJLEN", b"{oc}chart", b"$"]).await),
        3
    );

    // e) The department directory rides a hash: HGETALL reads flat,
    //    HKEYS/HVALS project one side, HSTRLEN sizes a field, HDEL
    //    retires one.
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"{oc}dir", b"core", b"anza", b"web", b"bo"]).await),
        2
    );
    let flat = rf(&a, &[b"HGETALL", b"{oc}dir"]).await;
    assert_eq!(bulk_at(flat.as_bytes(), 0), "core");
    assert_eq!(bulk_at(flat.as_bytes(), 1), "anza");
    let hk = rf(&a, &[b"HKEYS", b"{oc}dir"]).await;
    assert_eq!(bulk_at(hk.as_bytes(), 0), "core");
    let hv = rf(&a, &[b"HVALS", b"{oc}dir"]).await;
    assert_eq!(bulk_at(hv.as_bytes(), 1), "bo");
    assert_eq!(int_of(&r(&a, &[b"HSTRLEN", b"{oc}dir", b"core"]).await), 4);
    assert_eq!(int_of(&r(&a, &[b"HDEL", b"{oc}dir", b"web"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"HLEN", b"{oc}dir"]).await), 1);

    // f) MySQL keeps the headcount rollup after the JSON move.
    ddl(
        &mut c,
        "CREATE TABLE hc (org VARCHAR(16) PRIMARY KEY, heads BIGINT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO hc (org, heads) VALUES ('acme-labs', 5)",
    )
    .await;
    assert_eq!(
        one(&mut c, "SELECT heads FROM hc WHERE org = 'acme-labs'").await,
        int(5)
    );
}
