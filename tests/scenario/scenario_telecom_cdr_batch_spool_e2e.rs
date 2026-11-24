//! Batch 11 scenario #112 -- a telecom CDR spool: one `/produce_batch`
//! call files 50 call-detail records (ids strictly ascending), `/range`
//! replays all 50 with byte-exact base64 payloads, group `billing`
//! consumes 10 and `/ack_batch` clears its PEL, the RESP plane sees the
//! same spool (`XLEN cdr112/q0` == 50), a `delay_ms=700` record is
//! invisible to a plain read but wakes a parked long poll, an empty
//! channel parks the full `wait_ms` budget, and the audit ledger is a
//! SQL table. One REAL node: RocksMQ + MySQL + RESP fronts.

use std::time::Duration;

use crate::common::mq::{b64, items_of, msgs_of, pending_count};
use crate::common::mysql::{connect_root, ddl, ints, run};
use crate::scenario_common;
use crate::scenario_common::{front_world, http, int_of, r, FrontSpec};

const CH: &str = "cdr112";
const GROUP: &str = "billing";
const N: usize = 50;

/// The spooled payload of record `i` (base64 of `cdr-000`..`cdr-049`).
fn payload(i: usize) -> String {
    b64(format!("cdr-{i:03}").as_bytes())
}

/// `/produce_batch` body for `n` records (channel + base64 body).
fn batch_json(n: usize) -> String {
    let items: Vec<serde_json::Value> = (0..n)
        .map(|i| serde_json::json!({"channel": CH, "body": payload(i)}))
        .collect();
    serde_json::to_string(&items).expect("serialize batch")
}

/// `/ack_batch` body for the given ids.
fn ack_json(ids: &[String]) -> String {
    let items: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| serde_json::json!({"channel": CH, "group": GROUP, "id": id}))
        .collect();
    serde_json::to_string(&items).expect("serialize acks")
}

#[tokio::test]
async fn cdr_batch_spool_range_ack_delay_and_audit_ledger() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        rocksmq: true,
        mysql: true,
        // `delay_ms` rows are exchanged by the due sweep, which is OFF
        // (0) in the production default: arm the 100ms rhythm so the
        // delayed record below actually lands.
        extra_yaml: "lite:\n  delay_sweep_ms: 100\n".to_string(),
        ..Default::default()
    };
    let (front, resp) = front_world("cdr112", &spec).await;
    let mq = front.rocksmq.clone();

    // a) One batch call files 50 records; every item answers (200, id)
    //    and the ids ascend strictly.
    let t = scenario_common::perf::now();
    let (status, body) = http(&mq, "POST", "/produce_batch", Some(&batch_json(N))).await;
    assert_eq!(status, 200, "{body}");
    let items = items_of(&body);
    assert_eq!(items.len(), N, "one result per record: {body}");
    let mut ids = Vec::with_capacity(N);
    for (i, (code, id)) in items.iter().enumerate() {
        assert_eq!(*code, 200, "record {i}: {id}");
        assert!(id.contains('-'), "id shape: {id}");
        ids.push(id.clone());
    }
    for pair in ids.windows(2) {
        assert!(pair[0] < pair[1], "ids must ascend: {pair:?}");
    }

    // b) `/range` replays the whole spool in id order, payloads intact.
    let (status, body) = http(&mq, "POST", "/range?channel=cdr112", None).await;
    assert_eq!(status, 200, "{body}");
    let msgs = msgs_of(&body);
    assert_eq!(msgs.len(), N, "range returns all {N}: {body}");
    for (i, (id, got)) in msgs.iter().enumerate() {
        assert_eq!(id, &ids[i], "range id {i}");
        assert_eq!(got, &payload(i), "range payload {i}");
    }

    // c) Group billing takes 10, `/ack_batch` clears them, the PEL is
    //    empty again.
    let (status, body) = http(
        &mq,
        "POST",
        "/consume?channel=cdr112&group=billing&n=10",
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let got = msgs_of(&body);
    assert_eq!(got.len(), 10, "ten records delivered: {body}");
    for (i, (id, got_payload)) in got.iter().enumerate() {
        assert_eq!(id, &ids[i], "consume id {i}");
        assert_eq!(got_payload, &payload(i), "consume payload {i}");
    }
    assert_eq!(
        pending_count(&mq, CH, GROUP).await,
        10,
        "the PEL holds the ten deliveries"
    );
    let ack_ids: Vec<String> = got.iter().map(|(id, _)| id.clone()).collect();
    let (status, body) = http(&mq, "POST", "/ack_batch", Some(&ack_json(&ack_ids))).await;
    assert_eq!(status, 200, "{body}");
    let acks = items_of(&body);
    assert_eq!(acks.len(), 10, "one ack result per id: {body}");
    for (i, (code, text)) in acks.iter().enumerate() {
        assert_eq!((*code, text.as_str()), (200, "ok"), "ack {i}");
    }
    let (status, body) = http(&mq, "POST", "/pending?channel=cdr112&group=billing", None).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""pending":0"#),
        "PEL drained by /ack_batch: {body}"
    );
    scenario_common::perf::within(
        "cdr112/50-batch+10-consume+10-ack+range",
        t,
        Duration::from_secs(3),
    );

    // d) The RESP plane reads the same spool.
    assert_eq!(
        int_of(&r(&resp, &[b"XLEN", b"cdr112/q0"]).await),
        N as i64,
        "the batch landed on cdr112/q0"
    );

    // e) Billing drains the rest of the spool, so the delayed record
    //    below is the ONLY new entry (XREADGROUP `>` serves entries past
    //    the group's delivered watermark).
    let (status, body) = http(
        &mq,
        "POST",
        "/consume?channel=cdr112&group=billing&n=100",
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let rest = msgs_of(&body);
    assert_eq!(rest.len(), N - 10, "the remaining {} records", N - 10);
    let rest_ids: Vec<String> = rest.iter().map(|(id, _)| id.clone()).collect();
    let (status, body) = http(&mq, "POST", "/ack_batch", Some(&ack_json(&rest_ids))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(items_of(&body).len(), N - 10, "one ack per drained record");
    assert_eq!(pending_count(&mq, CH, GROUP).await, 0, "PEL empty again");

    // f) delay_ms=700: invisible to a plain read, then the parked long
    //    poll returns it once the due sweep exchanges the row.
    let item = serde_json::json!({"channel": CH, "delay_ms": 700, "body": payload(99)});
    let items = serde_json::to_string(&[item]).expect("serialize delayed batch");
    let (status, body) = http(&mq, "POST", "/produce_batch", Some(&items)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        items_of(&body)[0].0,
        200,
        "delayed produce accepted: {body}"
    );
    let (status, body) = http(
        &mq,
        "POST",
        "/consume?channel=cdr112&group=billing&n=1",
        None,
    )
    .await;
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"msgs":[]}"#),
        "pre-due read"
    );
    // The park budget is 1500ms, not 700ms: the row is due 700ms after
    // the produce, but the exchange itself happens on the NEXT 100ms
    // sweep tick (700..800ms), so a 700ms park could expire first and
    // answer dry. 1500ms keeps the wake deterministic while the
    // at_least(600ms) gate below still proves the request really parked.
    let parked = scenario_common::perf::now();
    let (status, body) = http(
        &mq,
        "POST",
        "/consume?channel=cdr112&group=billing&n=1&wait_ms=1500",
        None,
    )
    .await;
    let woken = msgs_of(&body);
    assert_eq!(status, 200, "{body}");
    assert_eq!(woken.len(), 1, "the parked read returns the record: {body}");
    assert_eq!(woken[0].1, payload(99), "delayed body intact");
    scenario_common::perf::at_least("cdr112/delay-park", parked, Duration::from_millis(600));
    scenario_common::perf::within("cdr112/delay-park-ceiling", parked, Duration::from_secs(3));

    // g) An empty channel parks the whole budget before answering dry.
    let parked = scenario_common::perf::now();
    let (status, body) = http(&mq, "POST", "/consume?channel=cdr112dry&wait_ms=700", None).await;
    assert_eq!((status, body.as_str()), (200, r#"{"msgs":[]}"#), "dry park");
    scenario_common::perf::at_least("cdr112/dry-park", parked, Duration::from_millis(600));
    scenario_common::perf::within("cdr112/dry-park-ceiling", parked, Duration::from_secs(2));

    // h) The audit ledger in SQL: one row per batch, count reconciled.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE cdr_audit(id BIGINT PRIMARY KEY, batch VARCHAR(16), n INT)",
    )
    .await;
    run(
        &mut c,
        &format!("INSERT INTO cdr_audit (id, batch, n) VALUES (1, '{CH}', {N})"),
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT n FROM cdr_audit WHERE id = 1").await,
        vec![N as i64],
        "audited record count"
    );
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM cdr_audit").await,
        vec![1],
        "one audit row"
    );
    scenario_common::perf::scenario("cdr112", t0);
}
