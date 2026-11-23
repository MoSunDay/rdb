//! Multi-store scenario #98 -- a border-screening desk on the SQL
//! plane driven by a live watchlist: `IN (SELECT ...)` flags travelers
//! from listed countries, `NOT IN` over a list holding a NULL pins the
//! 3-valued trap (every predicate is UNKNOWN, zero rows), the NULL-
//! guarded variant releases the clean travelers, EXISTS/NOT EXISTS run
//! both uncorrelated and correlated against the outer row, and scalar
//! subqueries project per-traveler counts (empty -> NULL). One REAL
//! node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply.iter().map(|r| r.iter().map(cell).collect()).collect()
}

#[tokio::test]
async fn border_desk_screens_travelers_through_a_watchlist() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("border98", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The traveler manifest and the watchlist. `country` on the
    //    watchlist is deliberately nullable: entry 3 is the stale NULL
    //    row that poisons NOT IN below.
    ddl(&mut c, "CREATE TABLE travelers (id INT PRIMARY KEY, name VARCHAR(24) NOT NULL, country VARCHAR(16) NOT NULL)").await;
    ddl(
        &mut c,
        "CREATE TABLE watchlist (id INT PRIMARY KEY, country VARCHAR(16))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO travelers (id, name, country) VALUES \
         (1, 'Ada', 'IR'), (2, 'Grace', 'US'), (3, 'Edsger', 'CU'), (4, 'Margaret', 'GB')",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO watchlist (id, country) VALUES (1, 'IR'), (2, 'CU'), (3, NULL)",
    )
    .await;

    // b) IN (SELECT ...): listed countries are held (NULLs in the list
    //    only turn misses into UNKNOWN, hits stay TRUE).
    let held = rows(
        &mut c,
        "SELECT name FROM travelers WHERE country IN (SELECT country FROM watchlist) ORDER BY id",
    )
    .await;
    assert_eq!(grid(&held), vec![vec!["Ada"], vec!["Edsger"]], "in-select");

    // c) NOT IN over the poisoned list: 3-valued logic drops EVERY row
    //    (x NOT IN (...NULL...) is UNKNOWN for any x).
    let none = rows(
        &mut c,
        "SELECT name FROM travelers WHERE country NOT IN (SELECT country FROM watchlist)",
    )
    .await;
    assert!(none.is_empty(), "null in list poisons not-in: {none:?}");

    // d) The NULL-guarded variant releases the clean travelers.
    let clean = rows(
        &mut c,
        "SELECT name FROM travelers WHERE country NOT IN \
         (SELECT country FROM watchlist WHERE country IS NOT NULL) ORDER BY id",
    )
    .await;
    assert_eq!(
        grid(&clean),
        vec![vec!["Grace"], vec!["Margaret"]],
        "guarded not-in"
    );

    // e) Uncorrelated EXISTS: the watchlist is non-empty, so the gate
    //    holds back the whole manifest.
    let all_gated = rows(
        &mut c,
        "SELECT name FROM travelers \
         WHERE EXISTS (SELECT 1 FROM watchlist WHERE country = 'IR') ORDER BY id",
    )
    .await;
    assert_eq!(grid(&all_gated).len(), 4, "uncorrelated exists holds all");
    let none_gated = rows(
        &mut c,
        "SELECT name FROM travelers WHERE NOT EXISTS (SELECT 1 FROM watchlist WHERE country = 'ZZ') ORDER BY id",
    )
    .await;
    assert_eq!(
        grid(&none_gated).len(),
        4,
        "not exists on empty hit set releases all"
    );

    // f) Correlated EXISTS / NOT EXISTS: the subquery probes the outer
    //    traveler's own country against the watchlist.
    let flagged = rows(
        &mut c,
        "SELECT t.name FROM travelers t WHERE EXISTS \
         (SELECT 1 FROM watchlist w WHERE w.country = t.country) ORDER BY t.id",
    )
    .await;
    assert_eq!(
        grid(&flagged),
        vec![vec!["Ada"], vec!["Edsger"]],
        "correlated exists"
    );
    let released = rows(
        &mut c,
        "SELECT t.name FROM travelers t WHERE NOT EXISTS \
         (SELECT 1 FROM watchlist w WHERE w.country = t.country) ORDER BY t.id",
    )
    .await;
    assert_eq!(
        grid(&released),
        vec![vec!["Grace"], vec!["Margaret"]],
        "correlated not exists"
    );

    // g) Scalar subqueries: a projected per-traveler probe (no hit ->
    //    NULL) and a MAX(id) keying in WHERE.
    let desks = rows(
        &mut c,
        "SELECT t.name, (SELECT w.id FROM watchlist w WHERE w.country = t.country) \
         FROM travelers t ORDER BY t.id",
    )
    .await;
    assert_eq!(
        grid(&desks),
        vec![
            vec!["Ada", "1"],
            vec!["Grace", "<NULL>"],
            vec!["Edsger", "2"],
            vec!["Margaret", "<NULL>"],
        ],
        "correlated scalar in select list"
    );
    let latest = rows(
        &mut c,
        "SELECT name FROM travelers WHERE id = (SELECT MAX(id) FROM travelers)",
    )
    .await;
    assert_eq!(grid(&latest), vec![vec!["Margaret"]], "scalar in where");
}
