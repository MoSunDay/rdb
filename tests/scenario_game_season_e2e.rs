//! Multi-store scenario #19 -- a game season close-out: the ladder is a
//! ZSET (ZINCRBY gains, ZREVRANGE crowns the champion order), the
//! champion's profile is a JSON doc bumped with NUMINCRBY, player
//! skill vectors live in a VectorSet (VADD :1 per player, VSIM recall
//! non-empty -- reco_vector shapes), the victory buff is a TTL string
//! reshaped by EXPIRE then immortalized by PERSIST, the MVP play rides
//! a stream, and the SQL settlement table must sum to the ladder.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, ints, run};
use scenario_common::{bulk_at, front_world, int_of, ok, r, rb, rf, FrontSpec};

#[tokio::test]
async fn game_season_ladder_vectors_buffs_and_settlement() {
    // a) One node: RESP + MySQL front; the settlement table mirrors the
    //    ladder.
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("game19", &spec).await;
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE season_scores (player VARCHAR(8) PRIMARY KEY, pts INT NOT NULL)").await;

    // b) The ladder: cumulative ZINCRBY gains settle at p1=150, p2=90,
    //    p3=45 -- ZREVRANGE must crown exactly that order.
    for (player, gain, want) in [("p1", 120, "120"), ("p2", 90, "90"), ("p1", 30, "150"), ("p3", 45, "45")] {
        let reply = r(&a, &[b"ZINCRBY", b"game:ladder", gain.to_string().as_bytes(), player.as_bytes()]).await;
        assert_eq!(bulk_at(&reply, 0), want, "{player} -> {want}");
    }
    let podium = rf(&a, &[b"ZREVRANGE", b"game:ladder", b"0", b"-1", b"WITHSCORES"]).await;
    assert!(podium.find("p1").unwrap() < podium.find("p2").unwrap(), "{podium}");
    assert!(podium.find("p2").unwrap() < podium.find("p3").unwrap(), "{podium}");
    assert_eq!(rb(&a, &[b"ZSCORE", b"game:ladder", b"p1"]).await, "150");

    // c) Champion profile: a JSON doc; the title bump is NUMINCRBY.
    ok(
        &r(&a, &[b"JSON.SET", b"game:profile:p1", b".", br#"{"name":"ace","wins":12}"#]).await,
        "profile set",
    );
    assert_eq!(bulk_at(&r(&a, &[b"JSON.NUMINCRBY", b"game:profile:p1", b".wins", b"1"]).await, 0), "13");
    let profile = rb(&a, &[b"JSON.GET", b"game:profile:p1", b"."]).await;
    assert!(profile.contains("13") && profile.contains("ace"), "{profile}");

    // d) Skill vectors: two players embedded, VSIM recalls the closest.
    assert_eq!(int_of(&r(&a, &[b"VADD", b"game:emb", b"VALUES", b"2", b"p1", b"1", b"0"]).await), 1, "vadd p1");
    assert_eq!(int_of(&r(&a, &[b"VADD", b"game:emb", b"VALUES", b"2", b"p2", b"0", b"1"]).await), 1, "vadd p2");
    let recall = rf(&a, &[b"VSIM", b"game:emb", b"COUNT", b"2", b"VALUES", b"1", b"0"]).await;
    assert_eq!(bulk_at(recall.as_bytes(), 0), "p1", "champion is nearest to the title run: {recall:?}");
    assert_eq!(bulk_at(recall.as_bytes(), 1), "p2", "runner-up recalled too: {recall:?}");

    // e) The victory buff: born plain, EXPIRE arms a TTL, PERSIST
    //    immortalizes it (TTL -> -1, key alive).
    ok(&r(&a, &[b"SET", b"game:buff:p1", b"shield"]).await, "buff granted");
    assert_eq!(int_of(&r(&a, &[b"EXPIRE", b"game:buff:p1", b"120"]).await), 1, "ttl armed");
    let ttl = int_of(&r(&a, &[b"TTL", b"game:buff:p1"]).await);
    assert!((1..=120).contains(&ttl), "buff ttl: {ttl}");
    assert_eq!(int_of(&r(&a, &[b"PERSIST", b"game:buff:p1"]).await), 1, "buff made permanent");
    assert_eq!(int_of(&r(&a, &[b"TTL", b"game:buff:p1"]).await), -1, "no ttl left");
    assert_eq!(rb(&a, &[b"GET", b"game:buff:p1"]).await, "shield", "buff intact");

    // f) The MVP play rides the season stream.
    let mvp = r(&a, &[b"XADD", b"season/mvp", b"*", b"player", b"p1", b"play", b"clutch"]).await;
    assert!(bulk_at(&mvp, 0).contains('-'), "mvp minted");
    assert_eq!(int_of(&r(&a, &[b"XLEN", b"season/mvp"]).await), 1);

    // g) Settlement: SQL mirrors the ladder and the sums must agree.
    run(&mut c, "INSERT INTO season_scores (player, pts) VALUES ('p1', 150), ('p2', 90), ('p3', 45)").await;
    let sql_sum = ints(&mut c, "SELECT SUM(pts) FROM season_scores").await;
    assert_eq!(sql_sum, vec![285]);
    let mut ladder_sum = 0i64;
    for p in ["p1", "p2", "p3"] {
        ladder_sum += rb(&a, &[b"ZSCORE", b"game:ladder", p.as_bytes()])
            .await
            .parse::<i64>()
            .unwrap();
    }
    assert_eq!(ladder_sum, 285, "ladder == SQL settlement");
    assert_eq!(
        ints(&mut c, "SELECT pts FROM season_scores WHERE player = 'p1'").await,
        vec![150],
        "the champion's row agrees with ZSCORE"
    );
}
