//! Multi-store scenario #85 -- an arcade leaderboard behind the ZADD
//! flag gauntlet: GT blocks score DROPS, LT blocks score RISES (both
//! still add brand-new members), CH counts updates as well as adds (a
//! same-score re-add counts as neither), INCR answers the new score as
//! a bulk and null when a flag vetoes the bump, NX never touches
//! existing members and XX never creates them, and the two mutex gates
//! (NX+XX; GT/LT/NX mixes) plus INCR's single-pair rule refuse with
//! their verbatim errors. One REAL node, RESP only.

use crate::scenario_common;
use scenario_common::{front_world, int_of, r, rb, rt, FrontSpec};

#[tokio::test]
async fn score_guard_gates_every_leaderboard_write() {
    let _serial = scenario_common::serial_guard().await;
    let (_front, a) = front_world("zg85", &FrontSpec::default()).await;
    let board = b"{zg85}board";
    let score = |m: &str| {
        let a = a.clone();
        let m = m.as_bytes().to_vec();
        async move { rb(&a, &[b"ZSCORE", board, &m]).await }
    };

    // a) The opening board: three players.
    assert_eq!(
        int_of(
            &r(
                &a,
                &[b"ZADD", board, b"100", b"alice", b"200", b"bob", b"150", b"carol"]
            )
            .await
        ),
        3
    );

    // b) GT guards the floor: a drop is skipped (:0 even with CH), a
    //    rise lands and counts under CH.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"GT", b"90", b"alice"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"GT", b"CH", b"90", b"alice"]).await),
        0
    );
    assert_eq!(score("alice").await, "100");
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"GT", b"CH", b"300", b"alice"]).await),
        1
    );
    assert_eq!(score("alice").await, "300");

    // c) LT guards the ceiling: a rise is skipped, a cut lands.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"LT", b"CH", b"400", b"alice"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"LT", b"CH", b"50", b"alice"]).await),
        1
    );
    assert_eq!(score("alice").await, "50");

    // d) CH counts changes, but a same-score re-add is neither.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"CH", b"50", b"alice"]).await),
        0
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"CH", b"75", b"alice"]).await),
        1
    );

    // e) XX never creates, NX never updates.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"XX", b"10", b"dave"]).await),
        0
    );
    assert_eq!(score("dave").await, "", "XX created nothing");
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"NX", b"10", b"dave"]).await),
        1
    );
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", board, b"NX", b"20", b"dave"]).await),
        0
    );
    assert_eq!(score("dave").await, "10", "NX never updates");

    // f) INCR: the bumped score as a bulk; a vetoed bump is null and
    //    the score holds.
    assert_eq!(
        rt(&a, &[b"ZADD", board, b"INCR", b"5", b"dave"])
            .await
            .trim_end(),
        "$2\r\n15"
    );
    assert!(r(&a, &[b"ZADD", board, b"NX", b"INCR", b"5", b"dave"])
        .await
        .starts_with(b"$-1"));
    assert_eq!(score("dave").await, "15");

    // g) The mutex gates and INCR's single-pair rule, verbatim.
    assert_eq!(
        rt(&a, &[b"ZADD", board, b"NX", b"XX", b"1", b"x"]).await,
        "-ERR XX and NX options at the same time are not compatible"
    );
    assert_eq!(
        rt(&a, &[b"ZADD", board, b"GT", b"LT", b"1", b"x"]).await,
        "-ERR GT, LT, and/or NX options at the same time are not compatible"
    );
    assert_eq!(
        rt(&a, &[b"ZADD", board, b"INCR", b"1", b"a", b"2", b"b"]).await,
        "-ERR INCR option supports a single increment-element pair"
    );
}
