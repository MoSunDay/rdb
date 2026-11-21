//! Multi-store scenario #61 -- playoff seeding desk: ZADD flag
//! semantics (GT/LT/CH/NX) decide whether a seeding update lands,
//! ZINCRBY adds round points, ZPOPMIN/ZPOPMAX flat pairs draw the
//! bracket ends, ZREVRANGEBYSCORE + LIMIT slices the title window and
//! ZUNIONSTORE aggregates the two group stages. MySQL keeps the final
//! standings. One REAL node, RESP + MySQL.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, int, one, run};
use scenario_common::{bulk_at, front_world, int_of, r, rb, rf, rt, FrontSpec};

#[tokio::test]
async fn seeding_flags_bracket_and_standings() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("sports61", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Round 1 seeds: GT refuses a LOWER seed number update, LT
    //    accepts it and CH counts the update as changed.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{sp}r1", b"4", b"ana", b"2", b"bo", b"7", b"cy", b"1", b"dee"]).await),
        4
    );
    assert_eq!(int_of(&r(&a, &[b"ZADD", b"{sp}r1", b"GT", b"1", b"bo"]).await), 0, "GT: 1 is worse than 2, refused");
    assert_eq!(rb(&a, &[b"ZSCORE", b"{sp}r1", b"bo"]).await, "2");
    assert_eq!(int_of(&r(&a, &[b"ZADD", b"{sp}r1", b"LT", b"CH", b"3", b"ana"]).await), 1, "LT+CH: 3 better than 4");
    assert_eq!(rb(&a, &[b"ZSCORE", b"{sp}r1", b"ana"]).await, "3");
    assert_eq!(int_of(&r(&a, &[b"ZADD", b"{sp}r1", b"NX", b"9", b"ana"]).await), 0, "NX: existing member ignored");
    assert_eq!(
        rt(&a, &[b"ZADD", b"{sp}r1", b"NX", b"GT", b"1", b"xx"]).await,
        "-ERR GT, LT, and/or NX options at the same time are not compatible",
        "NX+GT conflict (rdb wording)"
    );

    // b) Round 2 seeds a fresh board; ZINCRBY carries round points.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{sp}r2", b"3", b"bo", b"4", b"cy", b"8", b"dee", b"5", b"eli"]).await),
        4
    );
    assert_eq!(rb(&a, &[b"ZINCRBY", b"{sp}r2", b"2", b"cy"]).await, "6", "ZINCRBY replies the new score");

    // c) The bracket draws both ends: ZPOPMIN/ZPOPMAX answer flat
    //    [member, score] pairs.
    let low = rf(&a, &[b"ZPOPMIN", b"{sp}r1"]).await;
    assert_eq!(bulk_at(low.as_bytes(), 0), "dee", "lowest seed plays first: {low}");
    assert_eq!(bulk_at(low.as_bytes(), 1), "1");
    let top = rf(&a, &[b"ZPOPMAX", b"{sp}r2", b"2"]).await;
    assert_eq!(bulk_at(top.as_bytes(), 0), "dee");
    assert_eq!(bulk_at(top.as_bytes(), 1), "8");
    assert_eq!(bulk_at(top.as_bytes(), 2), "cy");

    // d) The title window: ZREVRANGEBYSCORE descends, LIMIT keeps one.
    assert_eq!(
        int_of(&r(&a, &[b"ZADD", b"{sp}pts", b"10", b"bo", b"12", b"cy", b"15", b"dee", b"8", b"eli"]).await),
        4
    );
    let title = rf(&a, &[b"ZREVRANGEBYSCORE", b"{sp}pts", b"12", b"8"]).await;
    assert_eq!(bulk_at(title.as_bytes(), 0), "cy", "desc within [8,12]: {title}");
    assert_eq!(bulk_at(title.as_bytes(), 1), "bo");
    assert_eq!(bulk_at(title.as_bytes(), 2), "eli");
    let champ = rf(&a, &[b"ZREVRANGEBYSCORE", b"{sp}pts", b"15", b"8", b"LIMIT", b"0", b"1"]).await;
    assert_eq!(bulk_at(champ.as_bytes(), 0), "dee", "LIMIT keeps the champion: {champ}");
    assert_eq!(int_of(&r(&a, &[b"ZRANK", b"{sp}pts", b"cy"]).await), 2, "0-based rank asc");

    // e) ZUNIONSTORE folds both boards into the Cup aggregate.
    assert_eq!(
        int_of(&r(&a, &[b"ZUNIONSTORE", b"{sp}cup", b"2", b"{sp}r1", b"{sp}r2"]).await),
        4
    );

    // f) MySQL mirrors the champion.
    ddl(
        &mut c,
        "CREATE TABLE standings (player VARCHAR(8) PRIMARY KEY, points BIGINT NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO standings (player, points) VALUES ('dee', 15)").await;
    assert_eq!(one(&mut c, "SELECT points FROM standings WHERE player = 'dee'").await, int(15));
}
