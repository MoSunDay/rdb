//! Multi-store scenario #75 -- ticket redelivery through the consumer
//! group machinery: XADDs on the bare parent topic answer
//! [auto-picked-stream, id], XGROUP CREATE registers the counter desk
//! at 0-0 (twice = BUSYGROUP), XREADGROUP COUNT rations new tickets to
//! alice and the tail to bob, XPENDING's summary aggregates
//! [total, min, max, consumer, count...] and its range form walks the
//! [id, consumer, idle, deliveries] rows, XAUTOCLAIM (min-idle 0)
//! steals alice's ticket for carol in the Redis>=7 three-segment shape
//! [cursor, entries, deleted-ids], and XACK drains the PEL back to
//! [0, nil, nil]. One REAL node, RESP only.

mod common;
mod scenario_common;

use common::lite::pel_rows;
use scenario_common::{bulk_at, front_world, int_of, r, rf, rt, stream_ids, FrontSpec};

#[tokio::test]
async fn ticket_queue_rations_pending_claims_and_acks() {
    let (_front, a) = front_world("redel75", &FrontSpec::default()).await;

    // a) Three tickets land on the bare parent topic; each reply is
    //    [queue-stream, id] and the parent pins ONE stream.
    let mut ids = Vec::new();
    let mut q = String::new();
    for i in 1..=3 {
        let reply = rf(&a, &[b"XADD", b"tickets", b"*", b"evt", format!("t{i}").as_bytes()]).await;
        let landed = bulk_at(reply.as_bytes(), 0);
        if q.is_empty() {
            q = landed.clone();
            assert!(q.starts_with("tickets/"), "auto-picked queue: {reply}");
        }
        assert_eq!(landed, q, "one queue per parent: {reply}");
        ids.push(bulk_at(reply.as_bytes(), 1));
    }
    let s = q.as_bytes();

    // b) The counter desk group at 0-0; a second CREATE refuses.
    assert_eq!(rt(&a, &[b"XGROUP", b"CREATE", s, b"desk", b"0-0"]).await, "+OK");
    assert_eq!(
        rt(&a, &[b"XGROUP", b"CREATE", s, b"desk", b"$"]).await,
        "-BUSYGROUP Consumer Group name already exists"
    );

    // c) Rationed delivery: alice takes two tickets, bob the tail.
    let alice = rf(&a, &[b"XREADGROUP", b"GROUP", b"desk", b"alice", b"COUNT", b"2", b"STREAMS", s, b">"]).await;
    assert_eq!(stream_ids(&alice), vec![ids[0].clone(), ids[1].clone()], "ration of two");
    let bob = rf(&a, &[b"XREADGROUP", b"GROUP", b"desk", b"bob", b"STREAMS", s, b">"]).await;
    assert_eq!(stream_ids(&bob), vec![ids[2].clone()], "tail to bob");
    assert_eq!(rf(&a, &[b"XREADGROUP", b"GROUP", b"desk", b"bob", b"STREAMS", s, b">"]).await, "*-1\r\n", "nothing new left");

    // d) XPENDING summary: [3, min, max, alice 2, bob 1] -- 7 cells (the
    //    consumer section is byte-sorted inline).
    let sum = rf(&a, &[b"XPENDING", s, b"desk"]).await;
    assert!(
        sum.starts_with("*7\r\n:3\r\n")
            && sum.contains(&format!("${}\r\n{}\r\n", ids[0].len(), ids[0]))
            && sum.contains(&format!("${}\r\n{}\r\n", ids[2].len(), ids[2]))
            && sum.contains("alice\r\n:2\r\n") && sum.contains("bob\r\n:1\r\n"),
        "summary aggregate: {sum}"
    );

    // e) XPENDING range rows: [id, consumer, idle, deliveries]; every
    //    ticket was delivered exactly once so far.
    let rows = pel_rows(rf(&a, &[b"XPENDING", s, b"desk", b"-", b"+", b"10"]).await.as_bytes());
    let seen: Vec<(String, String, String)> = rows
        .iter()
        .map(|row| {
            assert!(row[4].starts_with(':'), "idle column integer: {row:?}");
            (row[1].clone(), row[3].clone(), row[5].clone())
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (ids[0].clone(), "alice".into(), ":1".into()),
            (ids[1].clone(), "alice".into(), ":1".into()),
            (ids[2].clone(), "bob".into(), ":1".into()),
        ],
        "range rows id-ordered"
    );

    // f) XAUTOCLAIM (min-idle 0): carol steals alice's first ticket --
    //    the Redis>=7 three segments [cursor 0-0, entries, deleted].
    let claim = rf(&a, &[b"XAUTOCLAIM", s, b"desk", b"carol", b"0", b"0-0"]).await;
    assert!(claim.starts_with("*3\r\n"), "three segments: {claim}");
    assert!(claim.contains("$3\r\n0-0\r\n"), "scan complete: {claim}");
    assert!(claim.contains(&ids[0]) && claim.contains("evt"), "entry frame rides: {claim}");
    assert!(claim.ends_with("*0\r\n"), "no dead-letter transfers: {claim}");

    // g) The steal shows in the PEL: ticket 1 now carol's, delivered 2x.
    let rows = pel_rows(rf(&a, &[b"XPENDING", s, b"desk", b"-", b"+", b"10"]).await.as_bytes());
    assert_eq!(rows[0][3], "carol", "owner changed: {:?}", rows[0]);
    assert_eq!(rows[0][5], ":2", "delivery count bumped: {:?}", rows[0]);

    // h) XACK drains the PEL one :1 at a time; the summary collapses
    //    to [0, nil, nil].
    for id in &ids {
        assert_eq!(int_of(&r(&a, &[b"XACK", s, b"desk", id.as_bytes()]).await), 1, "ack {id}");
    }
    assert_eq!(
        rf(&a, &[b"XPENDING", s, b"desk"]).await,
        "*3\r\n:0\r\n$-1\r\n$-1\r\n",
        "pel drained"
    );
    assert_eq!(int_of(&r(&a, &[b"XLEN", s]).await), 3, "entries stay for history");
    assert_eq!(
        rf(&a, &[b"XREADGROUP", b"GROUP", b"desk", b"alice", b"COUNT", b"1", b"STREAMS", s, b">"]).await,
        "*-1\r\n",
        "acked history never redelivers"
    );
}
