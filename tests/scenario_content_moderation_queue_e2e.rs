//! Multi-store scenario #43 -- a content moderation pipeline: user
//! reports flow in over the RocksMQ HTTP front (/produce), a reviewer
//! consumes the batch (base64 payloads), triages into two RESP lists,
//! LMPOP drains the first non-empty queue, ZREMRANGEBYRANK keeps only
//! the top-severity backlog, SPOP samples a random double-check, and
//! MySQL keeps the verdict ledger. One REAL node, RESP + RocksMQ +
//! MySQL.

mod common;
mod scenario_common;

use common::mq::msgs_of;
use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::codec::b64d;
use scenario_common::{bulk_at, front_world, int_of, mq_post, r, rb, rf, FrontSpec};

#[tokio::test]
async fn moderation_ingests_triages_and_drains() {
    let spec = FrontSpec {
        mysql: true,
        rocksmq: true,
        ..Default::default()
    };
    let (front, a) = front_world("mod43", &spec).await;
    let (mq, resp) = (front.rocksmq.clone(), a.clone());
    let mut c = connect_root(&front.node).await;

    // a) Five reports ride the channel.
    let reports = [
        (r#"{"id":"r1","sev":9,"body":"spam storm"}"#, "r1", 9),
        (r#"{"id":"r2","sev":3,"body":"mild snark"}"#, "r2", 3),
        (r#"{"id":"r3","sev":7,"body":"dox attempt"}"#, "r3", 7),
        (r#"{"id":"r4","sev":1,"body":"false alarm"}"#, "r4", 1),
        (r#"{"id":"r5","sev":8,"body":"scam link"}"#, "r5", 8),
    ];
    for (line, _, _) in &reports {
        let (status, body) = mq_post(&mq, "/produce?channel=reports", line).await;
        assert_eq!(status, 200, "{body}");
    }

    // b) The reviewer consumes the batch: base64 payloads decode back
    //    to the JSON bodies.
    let (status, body) = mq_post(&mq, "/consume?channel=reports&group=mods&n=10", "").await;
    assert_eq!(status, 200, "{body}");
    let items: Vec<(String, String)> = msgs_of(&body)
        .into_iter()
        .map(|(id, b64)| (id, b64d(&b64)))
        .collect();
    assert_eq!(items.len(), 5, "whole batch drained: {body}");
    let joined = items.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>().join(" ");
    assert!(joined.contains("spam storm") && joined.contains("scam link"), "{joined}");

    // c) Triage: sev >= 5 goes to the urgent list, the rest to
    //    casual. LPUSH replies the new LENGTH, so the counter climbs.
    let mut urgent_len = 0i64;
    for (_, id, sev) in reports.iter().filter(|(_, _, s)| *s >= 5) {
        urgent_len += 1;
        assert_eq!(
            int_of(&r(&resp, &[b"LPUSH", b"{mod}urgent", id.as_bytes()]).await),
            urgent_len,
            "push {id}"
        );
        let _ = sev;
    }
    assert_eq!(urgent_len, 3);
    assert_eq!(
        int_of(&r(&resp, &[b"RPUSH", b"{mod}casual", b"r2", b"r4"]).await),
        2
    );

    // d) LMPOP drains the FIRST non-empty key of the pair: urgent
    //    wins; RIGHT pops the list tail -- the OLDEST push (r1 went in
    //    first, so [r5, r3, r1] pops r1).
    let first = rf(&resp, &[b"LMPOP", b"2", b"{mod}urgent", b"{mod}casual", b"RIGHT"]).await;
    assert!(first.contains("urgent"), "urgent is the hit queue: {first}");
    assert!(first.contains("r1"), "RIGHT pops the newest urgent: {first}");
    assert_eq!(int_of(&r(&resp, &[b"LLEN", b"{mod}urgent"]).await), 2);

    // e) Severity backlog: ZADD all five, then keep only the top three
    //    by rank (ZREMRANGEBYRANK 0 -4 strips everything above rank 3).
    for (_, id, sev) in &reports {
        let sev = sev.to_string();
        assert_eq!(
            int_of(&r(&resp, &[b"ZADD", b"{mod}backlog", sev.as_bytes(), id.as_bytes()]).await),
            1
        );
    }
    assert_eq!(
        int_of(&r(&resp, &[b"ZREMRANGEBYRANK", b"{mod}backlog", b"0", b"-4"]).await),
        2
    );
    let kept = rf(&resp, &[b"ZRANGE", b"{mod}backlog", b"0", b"-1"]).await;
    let kept_ids: Vec<String> = (0..3).map(|i| bulk_at(kept.as_bytes(), i)).collect();
    assert_eq!(kept_ids, vec!["r3", "r5", "r1"], "top severity kept, ascending");

    // f) SPOP samples one kept item for human double-check (any of the
    //    three is fine; the set shrinks by one).
    for id in ["r1", "r3", "r5"] {
        assert_eq!(int_of(&r(&resp, &[b"SADD", b"{mod}kept", id.as_bytes()]).await), 1);
    }
    let popped = rb(&resp, &[b"SPOP", b"{mod}kept"]).await;
    assert!(["r1", "r3", "r5"].contains(&popped.as_str()), "{popped}");
    assert_eq!(int_of(&r(&resp, &[b"SCARD", b"{mod}kept"]).await), 2);

    // g) Verdict ledger + ack the consumed batch so the group is clean.
    ddl(
        &mut c,
        "CREATE TABLE verdicts (id BIGINT PRIMARY KEY, report VARCHAR(8) NOT NULL, \
         verdict VARCHAR(16) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO verdicts (id, report, verdict) VALUES \
                 (1, 'r1', 'removed'), (2, 'r3', 'removed'), (3, 'r5', 'removed'), \
                 (4, 'r2', 'kept'), (5, 'r4', 'kept')").await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM verdicts WHERE verdict = 'removed'").await,
        vec![3]
    );
    for (id, _) in &items {
        let (status, body) =
            mq_post(&mq, &format!("/ack?channel=reports&group=mods&id={id}"), "").await;
        assert_eq!(status, 200, "{body}");
    }
}
