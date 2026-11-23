//! Multi-store scenario #108 -- the charity donation audit on the SQL
//! plane: SUM vs SUM(DISTINCT) exposes the double-count (repeat
//! donations of the same amount), COUNT(*) vs COUNT(DISTINCT) the
//! donor roster, MIN/MAX run over STRINGS lexicographically ('%'
//! (0x25) sorts under '2'), LIKE escapes match LITERAL wildcards
//! (`\%` / `\_`, sqlparser's MySQL pass-through), and an empty SUM is
//! NULL, not zero. One REAL node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, ints, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn donation_audit_dedupes_amounts_and_escapes_like_wildcards() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("don108", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Four donations: two donors repeat the SAME amount (100), one
    //    ref code carries a literal % (the escape probe below).
    ddl(
        &mut c,
        "CREATE TABLE donations (id INT PRIMARY KEY, donor VARCHAR(8) NOT NULL, \
        ref_code VARCHAR(16) NOT NULL, amount INT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        r"INSERT INTO donations (id, donor, ref_code, amount) VALUES
        (1, 'ali_ce', 'DON%24A', 100), (2, 'ali_de', 'DON%24B', 100),
        (3, 'alice', 'DON24C', 50), (4, 'ali_ce', 'DON%24D', 100)",
    )
    .await;

    // b) Totals: the raw SUM (350) double-counts the repeated 100s;
    //    SUM(DISTINCT) collapses to the two distinct amounts (150).
    let got = rows(
        &mut c,
        "SELECT SUM(amount), SUM(DISTINCT amount), COUNT(*), \
         COUNT(DISTINCT donor), COUNT(DISTINCT amount) FROM donations",
    )
    .await;
    let cells: Vec<String> = got[0].iter().map(cell).collect();
    assert_eq!(cells, vec!["350", "150", "4", "3", "2"], "sum vs distinct");

    // c) MIN/MAX over STRINGS are lexicographic: the underscore
    //    (0x5F) sorts under letters, so both ali_* handles rank
    //    before 'alice' ('alice' is the MAX), and the literal-% ref
    //    codes sort under the plain-digit ones ('%' 0x25 < '2').
    let got = rows(
        &mut c,
        "SELECT MIN(donor), MAX(donor), MIN(ref_code), MAX(ref_code) FROM donations",
    )
    .await;
    let cells: Vec<String> = got[0].iter().map(cell).collect();
    assert_eq!(
        cells,
        vec!["ali_ce", "alice", "DON%24A", "DON24C"],
        "lexicographic min/max"
    );

    // d) LIKE escapes: `DON\%%` (escaped % + trailing wildcard)
    //    matches only refs starting with a LITERAL percent (three
    //    rows), while `DON%` is the wildcard prefix (all four);
    //    `ali\_%` needs the underscore, so 'alice' (no underscore)
    //    is out.
    assert_eq!(
        ints(
            &mut c,
            r"SELECT COUNT(*) FROM donations WHERE ref_code LIKE 'DON\%%'"
        )
        .await,
        vec![3],
        "literal % matches"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM donations WHERE ref_code LIKE 'DON%'"
        )
        .await,
        vec![4],
        "wildcard % matches"
    );
    assert_eq!(
        ints(
            &mut c,
            r"SELECT COUNT(*) FROM donations WHERE donor LIKE 'ali\_%'"
        )
        .await,
        vec![3],
        "literal _ matches"
    );
    assert_eq!(
        ints(
            &mut c,
            r"SELECT id FROM donations WHERE donor LIKE 'ali\__e' ORDER BY id"
        )
        .await,
        vec![1, 2, 4],
        "escaped _ plus wildcard _"
    );

    // e) Per-donor roll-up: ali_ce's three 100s collapse under
    //    DISTINCT to 100 while the raw SUM says 200.
    let got = rows(
        &mut c,
        "SELECT donor, SUM(amount), SUM(DISTINCT amount) FROM donations \
         GROUP BY donor ORDER BY donor",
    )
    .await;
    let grid: Vec<Vec<String>> = got.iter().map(|r| r.iter().map(cell).collect()).collect();
    assert_eq!(
        grid,
        vec![
            vec!["ali_ce", "200", "100"],
            vec!["ali_de", "100", "100"],
            vec!["alice", "50", "50"],
        ],
        "per-donor dedup"
    );

    // f) An empty SUM is NULL, not zero (the empty audit window).
    let got = rows(&mut c, "SELECT SUM(amount) FROM donations WHERE id > 100").await;
    assert_eq!(cell(&got[0][0]), "<NULL>", "empty sum is null");
}
