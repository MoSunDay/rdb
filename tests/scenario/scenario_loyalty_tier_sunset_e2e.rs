//! Multi-store scenario #48 -- a loyalty program sunset: members ride
//! a ZSET of lifetime points, ZREVRANGEBYSCORE picks the platinum
//! band, ZRANDMEMBER draws the farewell lottery, ZSCAN walks the whole
//! membership under the cursor, SUNIONSTORE merges the tier perk sets,
//! and EXPIREAT sunsets the whole program (PTTL proves the clock).
//! One REAL node, RESP only.

use crate::scenario_common;
use scenario_common::{front_world, int_of, r, rf, FrontSpec};

#[tokio::test]
async fn loyalty_sunset_draws_merges_and_expires() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec::default();
    let (_front, a) = front_world("loyl48", &spec).await;

    // a) The membership: points -> member.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"ZADD", b"{ly}pts", b"9500", b"ada", b"8200", b"bob", b"7400", b"cy", b"5200",
                    b"dee", b"3100", b"eli"
                ]
            )
            .await
        ),
        5
    );

    // b) The platinum band: top scores, descending output.
    let plat = rf(&a, &[b"ZREVRANGEBYSCORE", b"{ly}pts", b"+inf", b"8000"]).await;
    assert!(
        plat.contains("ada") && plat.contains("bob") && !plat.contains("eli"),
        "{plat}"
    );

    // c) Farewell lottery: any of the five can win; the member stays
    //    in the set (draw without removal).
    let winner = scenario_common::rb(&a, &[b"ZRANDMEMBER", b"{ly}pts"]).await;
    assert!(
        ["ada", "bob", "cy", "dee", "eli"].contains(&winner.as_str()),
        "{winner}"
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZCARD", b"{ly}pts"]).await),
        5,
        "draw is non-destructive"
    );

    // d) ZSCAN walks the whole membership under a cursor (full page
    //    ends the walk at "0").
    let page = rf(
        &a,
        &[b"ZSCAN", b"{ly}pts", b"0", b"COUNT", b"100", b"WITHSCORES"],
    )
    .await;
    for m in ["ada", "bob", "cy", "dee", "eli"] {
        assert!(page.contains(m), "zscan sees {m}: {page}");
    }
    assert!(
        page.contains("9500") && page.contains("3100"),
        "scores ride the page"
    );

    // e) Perk sets merge: lounge + upgrades = the farewell bundle.
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{ly}lounge", b"p1", b"p2"]).await),
        2
    );
    assert_eq!(
        int_of(&r(&a, &[b"SADD", b"{ly}upgrades", b"p2", b"p3"]).await),
        2
    );
    assert_eq!(
        int_of(
            &r(
                &a,
                &[
                    b"SUNIONSTORE",
                    b"{ly}bundle",
                    b"{ly}lounge",
                    b"{ly}upgrades"
                ]
            )
            .await
        ),
        3
    );
    assert_eq!(int_of(&r(&a, &[b"SCARD", b"{ly}bundle"]).await), 3);

    // f) Sunset: EXPIREAT (unix seconds, 2s out) kills the program;
    //    PTTL shows a live clock first.
    let due = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 2;
    assert_eq!(
        int_of(&r(&a, &[b"EXPIREAT", b"{ly}pts", due.to_string().as_bytes()]).await),
        1
    );
    let ms = int_of(&r(&a, &[b"PTTL", b"{ly}pts"]).await);
    assert!(ms > 0 && ms <= 2000, "clock live: {ms}");
    tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
    assert_eq!(
        int_of(&r(&a, &[b"EXISTS", b"{ly}pts"]).await),
        0,
        "program sunset"
    );
    // The merged perk bundle outlives the points zset.
    assert_eq!(int_of(&r(&a, &[b"EXISTS", b"{ly}bundle"]).await), 1);
}
