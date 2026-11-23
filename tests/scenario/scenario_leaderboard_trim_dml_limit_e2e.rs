//! Multi-store scenario #105 -- the arcade leaderboard on the SQL
//! plane: UPDATE ORDER BY ... LIMIT boosts exactly the TOP-N rows
//! (affected count + survivor set), DELETE ORDER BY ... LIMIT trims
//! the tail k laggards, LIMIT 0 is a clean no-op, and a WHERE + ORDER
//! BY + LIMIT combo narrows before it slices. The literal (non
//! placeholder) spelling the qsem suite only drove prepared. One REAL
//! node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{aff, connect_root, ddl, ints, run};
use scenario_common::{front_world, FrontSpec};

#[tokio::test]
async fn leaderboard_boosts_top_n_and_trims_the_tail() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("board105", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Six players, strictly ordered scores.
    ddl(
        &mut c,
        "CREATE TABLE board (id INT PRIMARY KEY, player VARCHAR(8) NOT NULL, \
        score INT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO board (id, player, score) VALUES \
        (1, 'ada', 50), (2, 'bob', 40), (3, 'cyd', 30), \
        (4, 'dee', 25), (5, 'eli', 20), (6, 'fer', 10)",
    )
    .await;

    // b) Season boost: only the top-2 by score take the +100 bonus.
    assert_eq!(
        aff(
            &mut c,
            "UPDATE board SET score = score + 100 ORDER BY score DESC LIMIT 2"
        )
        .await,
        2,
        "top-2 boosted"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT id FROM board WHERE score >= 140 ORDER BY id"
        )
        .await,
        vec![1, 2],
        "survivor set: exactly ids 1,2 boosted"
    );
    assert_eq!(
        ints(&mut c, "SELECT score FROM board ORDER BY score DESC").await,
        vec![150, 140, 30, 25, 20, 10],
        "rest untouched"
    );

    // c) WHERE narrows first, then ORDER BY + LIMIT slices: of the
    //    two boosted rows only the single best takes a -5 correction.
    assert_eq!(
        aff(
            &mut c,
            "UPDATE board SET score = score - 5 WHERE score > 100 \
         ORDER BY score DESC LIMIT 1"
        )
        .await,
        1
    );
    assert_eq!(
        ints(&mut c, "SELECT score FROM board WHERE id = 1").await,
        vec![145],
        "only the top row corrected"
    );

    // d) Tail trim: DELETE the two lowest scores (the laggards).
    assert_eq!(
        aff(&mut c, "DELETE FROM board ORDER BY score ASC LIMIT 2").await,
        2
    );
    assert_eq!(
        ints(&mut c, "SELECT id FROM board ORDER BY id").await,
        vec![1, 2, 3, 4],
        "tail two gone"
    );

    // e) LIMIT 0 is a clean no-op: zero affected, board unchanged.
    assert_eq!(
        aff(
            &mut c,
            "UPDATE board SET score = 0 ORDER BY score DESC LIMIT 0"
        )
        .await,
        0
    );
    assert_eq!(
        aff(&mut c, "DELETE FROM board ORDER BY id DESC LIMIT 0").await,
        0
    );
    assert_eq!(
        ints(&mut c, "SELECT score FROM board ORDER BY score DESC").await,
        vec![145, 140, 30, 25],
        "untouched after LIMIT 0 pair"
    );
}
