//! Multi-store scenario #82 -- an incident replay desk on one stream:
//! XREADGROUP rations the day's events to two consumers, XINFO
//! CONSUMERS answers the roster as flat 6-element arrays (name/pending/
//! idle triples), then XGROUP SETID rewinds the group to 0-0 so `>`
//! readers replay already-consumed history to a fresh auditor (the
//! replayed PEL rows move with the entries, Redis semantics),
//! DELCONSUMER retires a worker and replies the number of pending
//! rows it purged (0 once gone), and the two failure shapes stay
//! verbatim (NOGROUP for a missing group, the Invalid stream ID
//! refusal for a malformed rewind point). One REAL node, RESP only.

use crate::scenario_common;
use scenario_common::{front_world, int_of, r, rf, rt, stream_ids, FrontSpec};

/// (name, pending) pairs of an XINFO CONSUMERS roster: tokenized on
/// CRLF, each consumer row is [.. "name" $len NAME "pending" :N ..].
fn roster(reply: &str) -> Vec<(String, String)> {
    let t: Vec<&str> = reply.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 < t.len() {
        if t[i] == "name" {
            // [name][$len][value][pending][:n][idle][:ms] -- the count
            // sits FIVE tokens past the field name.
            out.push((t[i + 2].to_string(), t[i + 5].to_string()));
            i += 9;
        } else {
            i += 1;
        }
    }
    out
}

#[tokio::test]
async fn replay_desk_rewinds_retires_and_shrinks_the_roster() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("rp82", &FrontSpec::default()).await;

    // a) The day's four events land on one auto-picked queue.
    let mut ids = Vec::new();
    let mut s = String::new();
    for i in 1..=4 {
        let reply = rf(
            &a,
            &[b"XADD", b"audit", b"*", b"evt", format!("e{i}").as_bytes()],
        )
        .await;
        if s.is_empty() {
            s = scenario_common::bulk_at(reply.as_bytes(), 0);
            assert!(s.starts_with("audit/"), "auto queue: {reply}");
        }
        ids.push(scenario_common::bulk_at(reply.as_bytes(), 1));
    }
    let stream = s.as_bytes();

    // b) The desk group; the day splits alice 2 / bob 2.
    assert_eq!(
        rt(&a, &[b"XGROUP", b"CREATE", stream, b"desk", b"0-0"]).await,
        "+OK"
    );
    for (who, lo, hi) in [("alice", 0, 2), ("bob", 2, 4)] {
        let got = stream_ids(
            &rf(
                &a,
                &[
                    b"XREADGROUP",
                    b"GROUP",
                    b"desk",
                    who.as_bytes(),
                    b"COUNT",
                    b"2",
                    b"STREAMS",
                    stream,
                    b">",
                ],
            )
            .await,
        );
        assert_eq!(got, ids[lo..hi], "{who} took {got:?}");
    }

    // c) The roster: two consumers, flat 6-element arrays, pending 2
    //    each, names sorted, idle an integer.
    let info = rf(&a, &[b"XINFO", b"CONSUMERS", stream, b"desk"]).await;
    assert!(info.starts_with("*2\r\n"), "two rows: {info}");
    assert_eq!(
        info.matches("*6\r\n").count(),
        2,
        "flat six-element rows: {info}"
    );
    assert!(
        info.contains("$4\r\nidle\r\n:"),
        "idle integer column: {info}"
    );
    assert_eq!(
        roster(&info),
        vec![("alice".into(), ":2".into()), ("bob".into(), ":2".into())],
        "{info}"
    );

    // d) Rewind to 0-0: +OK, then `>` hands the day's FIRST HALF to the
    //    auditor carol -- acked history replays, entries stay at 4, and
    //    (Redis semantics) the redelivered PEL rows now name carol.
    assert_eq!(
        rt(&a, &[b"XGROUP", b"SETID", stream, b"desk", b"0-0"]).await,
        "+OK"
    );
    let replay = stream_ids(
        &rf(
            &a,
            &[
                b"XREADGROUP",
                b"GROUP",
                b"desk",
                b"carol",
                b"COUNT",
                b"2",
                b"STREAMS",
                stream,
                b">",
            ],
        )
        .await,
    );
    assert_eq!(replay, ids[..2], "replayed head: {replay:?}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", stream]).await), 4);

    // e) The two failure shapes: a missing group, a malformed id.
    assert_eq!(
        rt(&a, &[b"XGROUP", b"SETID", stream, b"ghost", b"0-0"]).await,
        "-NOGROUP No such key 'audit/q0' or consumer group 'ghost'"
    );
    assert_eq!(
        rt(&a, &[b"XGROUP", b"SETID", stream, b"desk", b"not-an-id"]).await,
        "-ERR Invalid stream ID specified as stream command argument"
    );

    // f) The post-replay roster: carol owns the replayed head (alice's
    //    rows moved with the entries), bob still holds the tail, and
    //    empty-history alice stays visible (registry rows linger).
    let mid = rf(&a, &[b"XINFO", b"CONSUMERS", stream, b"desk"]).await;
    assert_eq!(
        roster(&mid),
        vec![
            ("alice".into(), ":0".into()),
            ("bob".into(), ":2".into()),
            ("carol".into(), ":2".into())
        ],
        "{mid}"
    );

    // g) DELCONSUMER retires bob with his 2 pending rows; alice retires
    //    with an empty PEL (0 purged); retiring bob again stays 0.
    assert_eq!(
        int_of(&r(&a, &[b"XGROUP", b"DELCONSUMER", stream, b"desk", b"bob"]).await),
        2
    );
    let after = rf(&a, &[b"XINFO", b"CONSUMERS", stream, b"desk"]).await;
    assert_eq!(
        roster(&after),
        vec![("alice".into(), ":0".into()), ("carol".into(), ":2".into())],
        "{after}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"XGROUP", b"DELCONSUMER", stream, b"desk", b"alice"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"XGROUP", b"DELCONSUMER", stream, b"desk", b"bob"]).await),
        0
    );
}
