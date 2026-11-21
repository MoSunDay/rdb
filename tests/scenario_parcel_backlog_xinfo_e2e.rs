//! Multi-store scenario #66 -- a parcel depot backlog audit: XADD on
//! the bare parent topic answers [topic, id], XINFO STREAM reads the
//! flat 8-field introspection (length / last-generated-id / groups /
//! idle-ms), XGROUP CREATE registers an audit group once (+OK, then
//! BUSYGROUP) and shows up in XINFO GROUPS, XREAD replays the backlog,
//! XTRIM MAXLEN compacts and XDEL retires single parcels, and XINFO of
//! a missing topic is refused. One REAL node, RESP only.

mod common;
mod scenario_common;

use scenario_common::{bulk_at, front_world, int_of, r, rf, rt, stream_ids, FrontSpec};

#[tokio::test]
async fn depot_backlog_introspection_and_compaction() {
    let spec = FrontSpec::default();
    let (_front, a) = front_world("parcel66", &spec).await;

    // a) Five parcels land on the bare parent topic: the reply is
    //    [queue-stream, id] -- the server picks the queue stream name.
    let mut ids = Vec::new();
    let mut stream = String::new();
    for i in 1..=5 {
        let reply = rf(&a, &[b"XADD", b"parcels", b"*", b"shelf", format!("s{i}").as_bytes(), b"bin", b"b3"]).await;
        let landed = bulk_at(reply.as_bytes(), 0);
        if stream.is_empty() {
            stream = landed.clone();
            assert!(stream.starts_with("parcels/"), "auto-picked queue stream: {reply}");
        }
        assert_eq!(landed, stream, "parent topic pins one queue: {reply}");
        ids.push(bulk_at(reply.as_bytes(), 1));
    }
    assert_eq!(ids.len(), 5);
    let q = stream.as_bytes();

    // b) XINFO STREAM introspects: a flat 8-field array whose VALUES
    //    are typed (counts reply :int, ids reply bulk), so the audit
    //    reads fields by name, not by bulk position.
    let info = rf(&a, &[b"XINFO", b"STREAM", q]).await;
    assert_eq!(int_line(&info, "length"), Some(5), "{info}");
    assert_eq!(bulk_line(&info, "last-generated-id"), ids[4], "{info}");
    assert_eq!(int_line(&info, "groups"), Some(0), "{info}");
    assert!(int_line(&info, "idle-ms").is_some(), "{info}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", q]).await), 5);

    // c) The auditors group registers once: +OK then BUSYGROUP; the
    //    group count moves to one and XINFO GROUPS carries its name
    //    and the delivered-id header.
    assert_eq!(rt(&a, &[b"XGROUP", b"CREATE", q, b"auditors", b"0-0"]).await, "+OK");
    assert!(rt(&a, &[b"XGROUP", b"CREATE", q, b"auditors", b"0-0"]).await.starts_with("-BUSYGROUP"));
    let info2 = rf(&a, &[b"XINFO", b"STREAM", q]).await;
    assert_eq!(int_line(&info2, "groups"), Some(1), "one group now: {info2}");
    let groups = rf(&a, &[b"XINFO", b"GROUPS", q]).await;
    assert!(groups.contains("auditors"), "{groups}");
    assert!(groups.contains("last-delivered-id"), "{groups}");

    // d) XREAD replays the whole backlog from 0-0.
    let replay = rf(&a, &[b"XREAD", b"STREAMS", q, b"0-0"]).await;
    assert_eq!(stream_ids(&replay), ids, "backlog order preserved");

    // e) Compaction: XTRIM MAXLEN keeps the newest two; XDEL retires
    //    one more; XLEN agrees all the way down.
    assert_eq!(int_of(&r(&a, &[b"XTRIM", q, b"MAXLEN", b"2"]).await), 3, "three oldest trimmed");
    assert_eq!(int_of(&r(&a, &[b"XLEN", q]).await), 2);
    assert_eq!(int_of(&r(&a, &[b"XDEL", q, ids[4].as_bytes()]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"XLEN", q]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"XDEL", q, ids[4].as_bytes()]).await), 0, "already retired");

    // f) A bare parent is refused (the full `parent/child` name is
    //    required) and a full name that never existed is no such key.
    assert_eq!(rt(&a, &[b"XINFO", b"STREAM", b"nosuchtopic"]).await, "-ERR a full stream name 'parent/child' is required");
    assert_eq!(rt(&a, &[b"XINFO", b"STREAM", b"nosuch/q7"]).await, "-ERR no such key");
}

/// The `:<int>` line directly after the named field line, parsed.
fn int_line(reply: &str, name: &str) -> Option<i64> {
    let mut it = reply.split("\r\n");
    while let Some(l) = it.next() {
        if l == name {
            return it.next().and_then(|v| v.strip_prefix(':')?.parse().ok());
        }
    }
    None
}

/// The bulk payload line after the named field line (its `$len` header
/// sits between the two).
fn bulk_line(reply: &str, name: &str) -> String {
    let mut it = reply.split("\r\n");
    while let Some(l) = it.next() {
        if l == name {
            let _len = it.next().unwrap_or("");
            return it.next().unwrap_or("").to_string();
        }
    }
    String::new()
}
