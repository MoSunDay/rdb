//! Multi-store scenario #73 -- a social follow-graph union feed: the
//! NON-STORE set algebra reads (SUNION/SINTER/SDIFF) fan the member
//! arrays out of same-slot operand sets (missing keys read as empty
//! sets; the CROSSSLOT gate fires on untagged pairs), SUNIONSTORE
//! materializes the same union for SCARD/SMISMEMBER to audit, and
//! SDIFF is directional (first set minus the rest). One REAL node,
//! RESP only.

mod common;
mod scenario_common;

use scenario_common::{bulk_at, front_world, int_of, r, rf, rt, FrontSpec};

/// Sorted member list of a set-reply (server order is unspecified, so
/// the audit sorts before comparing; SMISMEMBER's integer flags are
/// parsed inline where they are used).
fn members(reply: &str) -> Vec<String> {
    let n: usize = reply
        .split("\r\n")
        .next()
        .unwrap_or("")
        .trim_start_matches('*')
        .parse()
        .unwrap_or(0);
    let mut v: Vec<String> = (0..n).map(|i| bulk_at(reply.as_bytes(), i)).collect();
    v.sort();
    v
}

#[tokio::test]
async fn follow_graph_union_intersection_and_directional_diff() {
    let (_front, a) = front_world("feed73", &FrontSpec::default()).await;

    // a) Two follow sets: vips follow u1..u3, newcomers u2..u4.
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{fd}vip", b"u1", b"u2", b"u3"]).await), 3);
    assert_eq!(int_of(&r(&a, &[b"SADD", b"{fd}new", b"u2", b"u3", b"u4"]).await), 3);

    // b) The non-store union: four faces, one round trip.
    let uni = rf(&a, &[b"SUNION", b"{fd}vip", b"{fd}new"]).await;
    assert_eq!(members(&uni), vec!["u1", "u2", "u3", "u4"], "union {uni}");

    // c) The shared followees (intersection) and the VIP-only face
    //    (directional diff: first minus the rest).
    assert_eq!(members(&rf(&a, &[b"SINTER", b"{fd}vip", b"{fd}new"]).await), vec!["u2", "u3"]);
    assert_eq!(members(&rf(&a, &[b"SDIFF", b"{fd}vip", b"{fd}new"]).await), vec!["u1"]);
    assert_eq!(members(&rf(&a, &[b"SDIFF", b"{fd}new", b"{fd}vip"]).await), vec!["u4"], "directional");

    // d) Missing keys read as empty sets: a union keeps the survivor,
    //    an intersection collapses to nothing.
    assert_eq!(members(&rf(&a, &[b"SUNION", b"{fd}vip", b"{fd}ghost"]).await), vec!["u1", "u2", "u3"]);
    assert_eq!(rf(&a, &[b"SINTER", b"{fd}vip", b"{fd}ghost"]).await, "*0\r\n", "empty set");
    assert_eq!(members(&rf(&a, &[b"SUNION", b"{fd}gone1", b"{fd}gone2"]).await), Vec::<String>::new());

    // e) The CROSSSLOT gate on the read variants too.
    assert_eq!(
        rt(&a, &[b"SUNION", b"{fd}vip", b"{other}new"]).await,
        "-ERR CROSSSLOT Keys in request don't hash to the same slot"
    );

    // f) SUNIONSTORE materializes the union; SCARD sizes it and
    //    SMISMEMBER batch-checks membership flags.
    assert_eq!(int_of(&r(&a, &[b"SUNIONSTORE", b"{fd}all", b"{fd}vip", b"{fd}new"]).await), 4);
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"{fd}all"]).await), 4);
    let flags = rf(&a, &[b"SMISMEMBER", b"{fd}all", b"u1", b"u4", b"u9"]).await;
    let got: Vec<&str> = flags.split("\r\n").filter(|l| l.starts_with(':')).collect();
    assert_eq!(got, vec![":1", ":1", ":0"], "u1/u4 in, u9 out, request order: {flags}");

    // g) The stored union equals the live read it came from.
    assert_eq!(members(&rf(&a, &[b"SUNION", b"{fd}all"]).await), vec!["u1", "u2", "u3", "u4"]);
}
