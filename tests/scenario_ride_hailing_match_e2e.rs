//! Multi-store scenario #44 -- a ride-hailing dispatch match: nearby
//! drivers ride a ZSET keyed by distance (ZRANGEBYSCORE picks the
//! radius band, ZCOUNT sizes it), the idle list gives the assignment
//! order (LPOS locates a driver), ZINCRBY accumulates ride heat,
//! HINCRBYFLOAT accrues the metered fare, and GETSET swaps the
//! dispatch token atomically (the old token invalidates). One REAL
//! node, RESP only, hash-tagged keys for the multi-key reads.

mod common;
mod scenario_common;

use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, FrontSpec};

#[tokio::test]
async fn dispatch_matches_the_nearest_idle_driver() {
    let spec = FrontSpec::default();
    let (_front, a) = front_world("ride44", &spec).await;

    // a) The proximity index: score = distance in km.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{rh}near", b"1.2", b"d1", b"2.5", b"d2", b"4.0", b"d3", b"6.5", b"d4"]).await),
        4
    );
    // b) The 5km radius band: three drivers, ascending distance.
    let band = rf(&a, &[b"ZRANGEBYSCORE", b"{rh}near", b"0", b"5"]).await;
    let in_band: Vec<String> = (0..3).map(|i| bulk_at(band.as_bytes(), i)).collect();
    assert_eq!(in_band, vec!["d1", "d2", "d3"], "radius band ascending");
    assert_eq!(int_of(&r(&a, &[b"ZCOUNT", b"{rh}near", b"0", b"5"]).await), 3);
    // Exclusive bound shaves the edge driver.
    assert_eq!(int_of(&r(&a, &[b"ZCOUNT", b"{rh}near", b"(4", b"+inf"]).await), 1);

    // c) Idle queue and ranks: d1 is nearest (rank 0), d4 farthest.
    assert_eq!(int_of(&r(&a, &[b"RPUSH", b"{rh}idle", b"d1", b"d2", b"d3", b"d4"]).await), 4);
    assert_eq!(int_of(&r(&a, &[b"ZRANK", b"{rh}near", b"d1"]).await), 0);
    assert_eq!(int_of(&r(&a, &[b"ZREVRANK", b"{rh}near", b"d1"]).await), 3);
    assert_eq!(rb(&a, &[b"ZRANK", b"{rh}near", b"ghost"]).await, "", "missing rank is nil");
    // LPOS finds d3's slot in the assignment list.
    assert_eq!(int_of(&r(&a, &[b"LPOS", b"{rh}idle", b"d3"]).await), 2);

    // d) d2 takes the ride: heat climbs, fare meters.
    assert_eq!(rb(&a, &[b"ZINCRBY", b"{rh}heat", b"5", b"d2"]).await, "5");
    assert_eq!(rb(&a, &[b"ZINCRBY", b"{rh}heat", b"2.5", b"d2"]).await, "7.5");
    assert_eq!(rb(&a, &[b"ZINCRBY", b"{rh}heat", b"1", b"d1"]).await, "1");
    assert_eq!(rb(&a, &[b"HINCRBYFLOAT", b"{rh}fare", b"d2", b"12.5"]).await, "12.5");
    assert_eq!(rb(&a, &[b"HINCRBYFLOAT", b"{rh}fare", b"d2", b"3.25"]).await, "15.75");

    // e) The dispatch token rotates atomically: GETSET returns the OLD
    //    token while installing the new one.
    ok_pub(&r(&a, &[b"SET", b"{rh}token", b"tok-1"]).await);
    assert_eq!(rb(&a, &[b"GETSET", b"{rh}token", b"tok-2"]).await, "tok-1");
    assert_eq!(rb(&a, &[b"GET", b"{rh}token"]).await, "tok-2");
    assert_eq!(rb(&a, &[b"GETSET", b"{rh}token", b"tok-3"]).await, "tok-2");

    // f) Assignment ledger: d2 leaves the idle list and the near band.
    assert_eq!(int_of(&r(&a, &[b"LREM", b"{rh}idle", b"1", b"d2"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"ZREM", b"{rh}near", b"d2"]).await), 1);
    assert_eq!(int_of(&r(&a, &[b"ZCOUNT", b"{rh}near", b"0", b"5"]).await), 2);
    let after = rf(&a, &[b"ZRANGEBYSCORE", b"{rh}near", b"0", b"5"]).await;
    let remain: Vec<String> = (0..2).map(|i| bulk_at(after.as_bytes(), i)).collect();
    assert_eq!(remain, vec!["d1", "d3"]);
    assert_eq!(int_of(&r(&a, &[b"LLEN", b"{rh}idle"]).await), 3);
}

/// SET replies +OK (single-line status); tiny local assert helper.
fn ok_pub(reply: &[u8]) {
    assert!(reply.starts_with(b"+OK"), "expected +OK, got {reply:?}");
}
