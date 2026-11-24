//! Batch 11 scenario #124 -- a telemetry ingest plane on lite topics:
//! three explicit queues back the `telemetry` parent (q0 5 / q1 3 /
//! q2 2 samples), `XINFO TOPICS` answers the flat `[child, length]`
//! pairs in child byte order, `XINFO STREAM` introspects one queue,
//! a consumer group drains q0 and XACK empties the PEL summary, and
//! the Rust-only `XINFO LITE` counters reflect the traffic
//! (`messages` counts appends, `acks` counts acknowledged entries).
//! MySQL holds the shard registry the router reads at boot. One REAL
//! node, RESP + MySQL front.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, s};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, int_of, ok, r, rt, FrontSpec};

/// `(child, length)` pairs of a flat `XINFO TOPICS` reply.
fn topic_pairs(reply: &str) -> Vec<(String, i64)> {
    let toks: Vec<&str> = reply.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < toks.len() {
        let bulk = toks[i]
            .strip_prefix('$')
            .and_then(|l| l.parse::<usize>().ok())
            .is_some_and(|n| n == toks[i + 1].len());
        if bulk {
            let depth = toks[i + 2]
                .strip_prefix(':')
                .and_then(|v| v.parse::<i64>().ok());
            if let Some(depth) = depth {
                out.push((toks[i + 1].to_string(), depth));
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    out
}

/// The `:<int>` line right after the named field of a flat reply.
fn int_field(reply: &str, field: &str) -> i64 {
    let mut it = reply.split("\r\n");
    while let Some(l) = it.next() {
        if l == field {
            return it
                .next()
                .and_then(|v| v.strip_prefix(':'))
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| panic!("field {field} has no int in {reply}"));
        }
    }
    panic!("field {field} missing from {reply}");
}

/// Full-drain reply with a TIGHT quiet gap: the shared `rf` waits
/// 250ms of silence per call, which alone would blow the ten-probe
/// budget (the fixture cost, not the engine's).
async fn drain(addr: &str, args: &[&[u8]]) -> String {
    crate::common::lite::text(
        &crate::common::lite::cmd_full_reply_wait(
            addr,
            crate::common::TOKEN,
            args,
            20,
            10_000,
        )
        .await,
    )
}

/// Every entry id of an XREADGROUP reply, in arrival order.
fn delivered_ids(reply: &str) -> Vec<String> {
    reply
        .split("\r\n")
        .filter(|t| {
            t.split_once('-').is_some_and(|(a, b)| {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
            })
        })
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn telemetry_shards_topics_pel_and_lite_counters() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("topic124", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: 10 explicit-queue XADDs (q0 5 / q1 3 / q2 2) plus
    //    10 XINFO probes across the three shards.
    let t = scenario_common::perf::now();
    let mut ids = Vec::new();
    for (queue, n) in [("telemetry/q0", 5u8), ("telemetry/q1", 3), ("telemetry/q2", 2)] {
        for i in 1..=n {
            let id = format!("1-{i}");
            let reply = r(
                &a,
                &[b"XADD", queue.as_bytes(), id.as_bytes(), b"temp", b"21"],
            )
            .await;
            assert_eq!(
                reply,
                format!("${}\r\n{id}", id.len()).as_bytes(),
                "{queue} sample {i}"
            );
            if queue.ends_with("q0") {
                ids.push(id);
            }
        }
    }
    let q0: &[u8] = b"telemetry/q0";
    let q1: &[u8] = b"telemetry/q1";
    let q2: &[u8] = b"telemetry/q2";
    let parent: &[u8] = b"telemetry";
    let probes: [&[&[u8]]; 10] = [
        &[b"XINFO", b"STREAM", q0],
        &[b"XINFO", b"STREAM", q1],
        &[b"XINFO", b"STREAM", q2],
        &[b"XINFO", b"TOPICS", parent],
        &[b"XINFO", b"LITE"],
        &[b"XINFO", b"HELP"],
        &[b"XINFO", b"TOPICS", parent],
        &[b"XINFO", b"STREAM", q0],
        &[b"XINFO", b"STREAM", q1],
        &[b"XINFO", b"STREAM", q2],
    ];
    let mut replies = Vec::new();
    for p in probes {
        let reply = drain(&a, p).await;
        assert!(!reply.is_empty(), "empty XINFO reply for {p:?}");
        replies.push(reply);
    }
    scenario_common::perf::within("topic124/10-xadd+10-xinfo", t, Duration::from_secs(2));

    // b) The hot-block probes already carried the shard facts: q0 has
    //    5 entries, q1 3, q2 2, and TOPICS lists the pairs in child
    //    byte order (the registry walk is key-ordered).
    assert_eq!(int_field(&replies[0], "length"), 5, "{}", replies[0]);
    assert_eq!(int_field(&replies[1], "length"), 3, "{}", replies[1]);
    assert_eq!(int_field(&replies[2], "length"), 2, "{}", replies[2]);
    let want = vec![
        ("q0".to_string(), 5),
        ("q1".to_string(), 3),
        ("q2".to_string(), 2),
    ];
    assert_eq!(topic_pairs(&replies[3]), want, "{}", replies[3]);
    assert_eq!(topic_pairs(&replies[6]), want, "{}", replies[6]);

    // c) The queue stream introspection is flat and typed: length 5,
    //    one last-generated-id, zero groups before any CREATE.
    let info = drain(&a, &[b"XINFO", b"STREAM", q0]).await;
    assert_eq!(int_field(&info, "length"), 5, "{info}");
    assert!(info.contains("last-generated-id"), "{info}");
    assert_eq!(int_field(&info, "groups"), 0, "{info}");

    // d) Consumer group: CREATE registers, `>` hands all five samples
    //    to c1, the PEL summary counts them, XACK empties it again.
    ok(&r(&a, &[b"XGROUP", b"CREATE", q0, b"auditors", b"0-0"]).await, "xgroup create");
    let got = delivered_ids(&drain(
        &a,
        &[
            b"XREADGROUP",
            b"GROUP",
            b"auditors",
            b"c1",
            b"COUNT",
            b"10",
            b"STREAMS",
            q0,
            b">",
        ],
    )
    .await);
    assert_eq!(got, ids, "q0 delivered in append order");
    let pending = drain(&a, &[b"XPENDING", q0, b"auditors"]).await;
    assert_eq!(pending.split("\r\n").nth(1), Some(":5"), "{pending}");
    let ack_args: Vec<&[u8]> = {
        let mut v = vec![b"XACK".as_slice(), q0, b"auditors".as_slice()];
        v.extend(ids.iter().map(|i| i.as_bytes()));
        v
    };
    assert_eq!(int_of(&r(&a, &ack_args).await), 5, "all five acked");
    let pending = drain(&a, &[b"XPENDING", q0, b"auditors"]).await;
    assert_eq!(pending.split("\r\n").nth(1), Some(":0"), "{pending}");

    // e) XINFO LITE: the flat counters -- 10 appends bumped `messages`,
    //    5 acks bumped `acks`; the live-stream and PEL-dirty gauges ride
    //    along in the same 10-element array.
    // The pre-ack probe already counts the ten appends (no ack yet).
    let lite = replies[4].clone();
    assert!(int_field(&lite, "messages") >= 10, "{lite}");
    assert_eq!(int_field(&lite, "acks"), 0, "{lite}");
    // After the five XACKs the same counters tell the whole story.
    let lite = drain(&a, &[b"XINFO", b"LITE"]).await;
    assert!(int_field(&lite, "messages") >= 10, "{lite}");
    assert!(int_field(&lite, "acks") >= 2, "{lite}");
    assert_eq!(int_field(&lite, "messages"), 10, "{lite}");
    assert_eq!(int_field(&lite, "acks"), 5, "{lite}");
    for field in ["streams-live", "streams-reaped", "offset-dirty"] {
        int_field(&lite, field);
    }

    // f) MySQL keeps the shard registry the router reads: one row per
    //    child queue with its live depth (cross-checked against the
    //    XINFO TOPICS pairs).
    ddl(
        &mut c,
        "CREATE TABLE shard_registry (shard VARCHAR(32) PRIMARY KEY, depth INT NOT NULL)",
    )
    .await;
    for (child, depth) in &want {
        c.query_drop(format!(
            "INSERT INTO shard_registry (shard, depth) VALUES ('telemetry/{child}', {depth})"
        ))
        .await
        .expect("register shard");
    }
    let registered = rows(&mut c, "SELECT shard, depth FROM shard_registry ORDER BY shard").await;
    assert_eq!(
        registered,
        vec![
            vec![s("telemetry/q0"), s("5")],
            vec![s("telemetry/q1"), s("3")],
            vec![s("telemetry/q2"), s("2")],
        ],
        "{registered:?}"
    );
    // A replayed registration is an UPSERT, not a 1062: the row-store
    // pk model re-inserts the same key in place (only a unique
    // SECONDARY index answers 1062 -- sql_composite_pk_e2e), so the
    // registry keeps exactly three rows and the newer depth wins.
    let aff = common::mysql::aff(
        &mut c,
        "INSERT INTO shard_registry (shard, depth) VALUES ('telemetry/q0', 7)",
    )
    .await;
    assert_eq!(aff, 1, "replayed registration reports one row");
    assert_eq!(
        rows(&mut c, "SELECT shard, depth FROM shard_registry ORDER BY shard").await,
        vec![
            vec![s("telemetry/q0"), s("7")],
            vec![s("telemetry/q1"), s("3")],
            vec![s("telemetry/q2"), s("2")],
        ],
        "upsert kept one row per shard"
    );
    assert_eq!(
        rt(&a, &[b"XLEN", q0]).await,
        ":5",
        "registry depth agrees with XLEN"
    );

    scenario_common::perf::scenario("topic124", t0);
}
