//! Multi-store scenario #102 -- the quarterly sales report on the SQL
//! plane, assembled from CTEs: a `WITH big` factored subquery joins
//! ITSELF (referenced twice under two aliases), a chained pair with a
//! column-alias list renames as it flows (later CTE sees the earlier
//! one), a subquery lives INSIDE a CTE body, a CTE feeds a UNION ALL
//! compound (kept duplicates, then deduped via a sibling UNION), and
//! WITH RECURSIVE rejects loudly with 1235. One REAL node, MySQL
//! front.

use crate::common;
use crate::scenario_common;
use common::mysql::{
    connect_root, ddl, int, rows, rows_ordered, run, server_error, ER_NOT_SUPPORTED_YET,
};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    MVal::NULL => "<NULL>".to_string(),
                    MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn sales_report_builds_on_ctes_chains_and_compounds() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("cte102", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The quarter's deal ledger: three fat deals (>30), one thin.
    ddl(
        &mut c,
        "CREATE TABLE deals (id INT PRIMARY KEY, region VARCHAR(8) NOT NULL, amount INT NOT NULL)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO deals (id, region, amount) VALUES \
        (1, 'east', 50), (2, 'west', 45), (3, 'east', 80), (4, 'north', 20)",
    )
    .await;

    // b) The CTE referenced twice: big deals joined against themselves
    //    on id+1 (adjacent fat deals).
    let self_join = rows_ordered(
        &mut c,
        "WITH big AS (SELECT id, amount FROM deals WHERE amount > 30) \
         SELECT a.id, b.id FROM big a JOIN big b ON a.id + 1 = b.id",
    )
    .await;
    assert_eq!(
        grid(&self_join),
        vec![vec!["1", "2"], vec!["2", "3"]],
        "cte self-join"
    );

    // c) Chain + column alias list: x renames id -> n, y (later) reads
    //    x (earlier) and shifts by ten.
    let chained = rows(
        &mut c,
        "WITH x (n) AS (SELECT id FROM deals WHERE id <= 2), \
         y (m) AS (SELECT n + 10 FROM x) SELECT m FROM y ORDER BY m",
    )
    .await;
    assert_eq!(
        grid(&chained),
        vec![vec!["11"], vec!["12"]],
        "alias list + chain"
    );

    // d) A subquery INSIDE the CTE body: regions holding a fat deal,
    //    then the deals of those regions.
    let via_sub = rows(
        &mut c,
        "WITH hit AS (SELECT id FROM deals WHERE region IN \
         (SELECT region FROM deals WHERE amount > 70)) SELECT COUNT(*) FROM hit",
    )
    .await;
    assert_eq!(via_sub, vec![vec![int(2)]], "subquery inside the CTE");

    // e) CTE + compound: the east roster keeps its duplicate under
    //    UNION ALL (4 rows) and dedups under plain UNION (2 rows).
    let all = rows_ordered(
        &mut c,
        "WITH east AS (SELECT id FROM deals WHERE region = 'east') \
         SELECT id FROM east UNION ALL SELECT id FROM east",
    )
    .await;
    assert_eq!(
        grid(&all),
        vec![vec!["1"], vec!["1"], vec!["3"], vec!["3"]],
        "union all keeps both copies"
    );
    let dedup = rows_ordered(
        &mut c,
        "WITH east AS (SELECT id FROM deals WHERE region = 'east') \
         SELECT id FROM east UNION SELECT id FROM east",
    )
    .await;
    assert_eq!(grid(&dedup), vec![vec!["1"], vec!["3"]], "union dedups");

    // f) WITH RECURSIVE is not supported: loud 1235 naming recursion.
    let e = server_error(&mut c, "WITH RECURSIVE r (n) AS (SELECT 1) SELECT n FROM r").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    assert!(
        e.message.to_lowercase().contains("recursive"),
        "{}",
        e.message
    );
}
