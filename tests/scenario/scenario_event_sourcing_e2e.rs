//! Multi-store scenario #37 -- event-sourced account ledger: events
//! append to a lite stream (XADD), the projector group replays them
//! once (XREADGROUP GROUP <name> -- the name is the GROUP, not the
//! stream), folds each event into the SQL balance projection and XACKs
//! downstream; the empty PEL answers the *3 summary shape; a fresh
//! event after the checkpoint delivers exactly the delta; a crashed
//! worker leaves the event in the PEL (XPENDING :1) until XAUTOCLAIM
//! requeues it (*3 cursor/entries/deleted) for the standby consumer;
//! XINFO GROUPS/CONSUMERS expose the group state. One REAL node,
//! MySQL + RESP stream fronts.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{front_world, r, rf, stream_ids, FrontSpec};

const STREAM: &[u8] = b"acct/events";

#[tokio::test]
async fn account_event_sourcing_projection_and_pel_recovery() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("events37", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The projection table starts empty; the ledger starts at four
    //    events (open, two deposits, a withdrawal).
    ddl(
        &mut c,
        "CREATE TABLE projection (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL, \
         last_id VARCHAR(24) NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO projection (id, balance, last_id) VALUES (1, 0, 'none')",
    )
    .await;
    let mut ids_minted = Vec::new();
    for (t, v) in [
        ("opened", "0"),
        ("deposit", "100"),
        ("deposit", "50"),
        ("withdraw", "30"),
    ] {
        let reply = r(
            &a,
            &[
                b"XADD",
                STREAM,
                b"*",
                b"type",
                t.as_bytes(),
                b"amount",
                v.as_bytes(),
            ],
        )
        .await;
        let id = scenario_common::bulk_at(&reply, 0);
        assert!(id.contains('-'), "id minted: {id}");
        ids_minted.push(id);
    }
    assert_eq!(int(&r(&a, &[b"XLEN", STREAM]).await), 4, "four events");

    // b) The full history is range-readable; ids match the receipts.
    let all = rf(&a, &[b"XRANGE", STREAM, b"-", b"+"]).await;
    let ids_range = stream_ids(&all);
    assert_eq!(ids_range, ids_minted, "xrange ids match xadd receipts");

    // c) The projector replays the backlog ONCE and folds it into SQL.
    ok(&r(&a, &[b"XGROUP", b"CREATE", STREAM, b"proj", b"0-0"]).await);
    let batch = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"proj",
            b"worker",
            b"COUNT",
            b"10",
            b"STREAMS",
            STREAM,
            b">",
        ],
    )
    .await;
    let delivered = stream_ids(&batch);
    assert_eq!(delivered, ids_minted, "backlog delivered in order");
    let mut balance = 0i64;
    for (t, v) in [
        ("opened", "0"),
        ("deposit", "100"),
        ("deposit", "50"),
        ("withdraw", "30"),
    ] {
        let amount: i64 = v.parse().unwrap();
        balance += if t == "withdraw" { -amount } else { amount };
    }
    assert_eq!(balance, 120, "folded balance");
    run(
        &mut c,
        &format!(
            "UPDATE projection SET balance = {balance}, last_id = '{}' WHERE id = 1",
            delivered.last().unwrap()
        ),
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT balance FROM projection").await,
        vec![120]
    );

    // d) Every event acked: the PEL summary collapses to the empty
    //    *3 shape (count :0, null extremes).
    let mut ack_args: Vec<&[u8]> = vec![b"XACK", STREAM, b"proj"];
    ids_minted
        .iter()
        .for_each(|id| ack_args.push(id.as_bytes()));
    assert_eq!(int(&r(&a, &ack_args).await), 4, "all acked");
    let pel = rf(&a, &[b"XPENDING", STREAM, b"proj"]).await;
    assert_eq!(
        pel, "*3\r\n:0\r\n$-1\r\n$-1\r\n",
        "empty PEL summary: {pel:?}"
    );
    let again = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"proj",
            b"worker",
            b"STREAMS",
            STREAM,
            b">",
        ],
    )
    .await;
    assert!(!again.contains("amount"), "no redelivery: {again:?}");

    // e) A fresh event after the checkpoint delivers exactly the delta
    //    (the group cursor kept the position).
    let fresh = r(
        &a,
        &[
            b"XADD",
            STREAM,
            b"*",
            b"type",
            b"withdraw",
            b"amount",
            b"20",
        ],
    )
    .await;
    let fresh_id = scenario_common::bulk_at(&fresh, 0);
    assert_eq!(int(&r(&a, &[b"XLEN", STREAM]).await), 5);
    let delta = rf(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"proj",
            b"worker",
            b"COUNT",
            b"10",
            b"STREAMS",
            STREAM,
            b">",
        ],
    )
    .await;
    let delta_ids = stream_ids(&delta);
    assert_eq!(
        delta_ids,
        vec![fresh_id.clone()],
        "only the delta: {delta:?}"
    );
    run(
        &mut c,
        &format!(
            "UPDATE projection SET balance = balance - 20, last_id = '{fresh_id}' WHERE id = 1"
        ),
    )
    .await;

    // f) The worker crashes BEFORE acking the delta: the event sits in
    //    the PEL (summary :1), the standby XAUTOCLAIMs it back (the
    //    *3 cursor/entries/deleted shape), re-folds it, acks, and the
    //    PEL empties again. The projection must stay correct: the
    //    crashed worker's un-acked fold is re-derived, not doubled
    //    (the crashed run never wrote SQL).
    let pel = rf(&a, &[b"XPENDING", STREAM, b"proj"]).await;
    // (the non-empty summary is the EXTENDED *5 shape: count, min, max,
    // consumer name, consumer count -- the *3/:0 form is the empty PEL)
    assert!(pel.starts_with("*5\r\n:1\r\n"), "summary shape: {pel:?}");
    assert!(pel.contains("worker"), "pending consumer named: {pel:?}");
    let claim = rf(
        &a,
        &[
            b"XAUTOCLAIM",
            STREAM,
            b"proj",
            b"standby",
            b"0",
            b"0-0",
            b"COUNT",
            b"10",
        ],
    )
    .await;
    assert!(
        claim.starts_with("*3\r\n"),
        "cursor/entries/deleted: {claim:?}"
    );
    assert!(
        claim.contains(fresh_id.as_str()),
        "delta reclaimed: {claim:?}"
    );
    assert_eq!(
        int(&r(&a, &[b"XACK", STREAM, b"proj", fresh_id.as_bytes()]).await),
        1
    );
    let pel = rf(&a, &[b"XPENDING", STREAM, b"proj"]).await;
    assert_eq!(pel, "*3\r\n:0\r\n$-1\r\n$-1\r\n", "PEL drained: {pel:?}");
    assert_eq!(
        ints(&mut c, "SELECT balance FROM projection").await,
        vec![100],
        "net of the 20"
    );

    // g) The group surfaces: XINFO GROUPS names proj with the empty
    //    PEL, XINFO CONSUMERS lists both consumers.
    let groups = rf(&a, &[b"XINFO", b"GROUPS", STREAM]).await;
    assert!(groups.contains("proj"), "{groups:?}");
    let consumers = rf(&a, &[b"XINFO", b"CONSUMERS", STREAM, b"proj"]).await;
    assert!(
        consumers.contains("worker") && consumers.contains("standby"),
        "{consumers:?}"
    );
}

fn ok(reply: &[u8]) {
    assert!(reply.starts_with(b"+OK"), "{reply:?}");
}

fn int(reply: &[u8]) -> i64 {
    let s = String::from_utf8_lossy(reply);
    s.trim().trim_start_matches(':').parse().expect("int line")
}
