//! Multi-store scenario #51 -- a feature flag rollout console: every
//! flag is a JSON doc (JSON.MGET batches the dashboard read, TYPE
//! verifies shape), the ramp percentage eases up with NUMINCRBY, HDEL
//! drops the kill-switch hash field, and MySQL audits every bump. One
//! REAL node, RESP + MySQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn flag_rollout_ramps_batches_and_kills() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("flag51", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Three flags (checkout at 0, search at 10, darkmode at 50) --
    //    the tag keeps the dashboard batch same-slot.
    for k in ["{ff}checkout", "{ff}search", "{ff}darkmode"] {
        let pct = match k {
            "{ff}search" => "10",
            "{ff}darkmode" => "50",
            _ => "0",
        };
        let doc = format!(r#"{{"pct":{pct},"owner":"core"}}"#);
        scenario_common::ok(&r(&a, &[b"JSON.SET", k.as_bytes(), b"$", doc.as_bytes()]).await, "set flag");
    }

    // b) The dashboard read: JSON.MGET over same-slot keys returns one
    //    value per key with a nil for the missing one.
    let board = rf(&a, &[b"JSON.MGET", b"{ff}checkout", b"{ff}search", b"{ff}darkmode", b"{ff}ghost", b"$.pct"]).await;
    assert!(board.contains("10") && board.contains("50"), "{board}");
    assert!(board.contains("$-1"), "ghost flag is nil: {board}");

    // c) Shape checks: the ramp is a number; the whole doc is an object.
    assert_eq!(rt(&a, &[b"JSON.TYPE", b"{ff}search", b"$.pct"]).await.trim_start_matches('+'), "integer");
    assert_eq!(rt(&a, &[b"JSON.TYPE", b"{ff}search", b"$"]).await.trim_start_matches('+'), "object");
    assert_eq!(int_of(&r(&a, &[b"JSON.STRLEN", b"{ff}search", b"$.owner"]).await), 4, "'core' is 4 chars");

    // d) The ramp eases up: 10 -> 25.5 -> 40.
    assert!(rb(&a, &[b"JSON.NUMINCRBY", b"{ff}search", b"$.pct", b"15.5"]).await.contains("25.5"));
    assert!(rb(&a, &[b"JSON.NUMINCRBY", b"{ff}search", b"$.pct", b"14.5"]).await.contains("40"));
    // INCRBYFLOAT on a plain string key mirrors the same semantics.
    scenario_common::ok(&r(&a, &[b"SET", b"{ff}heat", b"5"]).await, "set heat");
    assert_eq!(rb(&a, &[b"INCRBYFLOAT", b"{ff}heat", b"0.25"]).await, "5.25");

    // e) The kill switch hash: one field per flag; HDEL flips
    //    checkout off and HEXISTS proves it gone.
    assert_eq!(
        int_of(&r(&a, &[b"HSET", b"{ff}kill", b"checkout", b"on", b"search", b"off"]).await),
        2
    );
    assert_eq!(int_of(&r(&a, &[b"HDEL", b"{ff}kill", b"checkout"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"HEXISTS", b"{ff}kill", b"checkout"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"HLEN", b"{ff}kill"]).await), 1);

    // f) Audit trail in MySQL mirrors the ramp.
    ddl(
        &mut c,
        "CREATE TABLE flag_audit (id BIGINT PRIMARY KEY, flag VARCHAR(32) NOT NULL, pct DECIMAL(8,2) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO flag_audit (id, flag, pct) VALUES \
                 (1, 'search', 10.00), (2, 'search', 25.50), (3, 'search', 40.00)").await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM flag_audit WHERE flag = 'search'").await,
        vec![3]
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM flag_audit WHERE pct > 30").await,
        vec![1]
    );
}
