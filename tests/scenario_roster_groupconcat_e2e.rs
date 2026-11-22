//! Multi-store scenario #87 -- a club-roster desk on the SQL plane:
//! GROUP_CONCAT rolls each team's members with the default comma, a
//! custom SEPARATOR, and DISTINCT dedup (multi-arg form), CONCAT_WS
//! glues roster lines while skipping NULL cells (separator immune to
//! the NULL), CONCAT keeps NULL destructive propagation, and REGEXP
//! filters names byte-wise case-sensitively. One REAL node, MySQL front.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, one, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

/// Render one SQL cell (the saga32 convention: NULL keeps its spelling).
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        MVal::Double(d) => d.to_string(),
        other => format!("{other:?}"),
    }
}

fn cells(rs: Vec<Vec<MVal>>) -> Vec<Vec<String>> {
    rs.into_iter().map(|r| r.iter().map(cell).collect()).collect()
}

#[tokio::test]
async fn roster_desk_concats_dedupes_and_regexes() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("rc87", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Six members across two teams; roles repeat within a team and
    //    one member has a NULL role.
    ddl(&mut c, "CREATE TABLE members (id BIGINT PRIMARY KEY, name VARCHAR(16) NOT NULL, team VARCHAR(16) NOT NULL, role VARCHAR(16))").await;
    run(&mut c, "INSERT INTO members (id, name, team, role) VALUES \
        (1,'ada','red','lead'),(2,'bob','red','crew'),(3,'cyd','red','lead'),\
        (4,'dee','blue','lead'),(5,'eve','blue',NULL),(6,'fang','blue','crew')").await;

    // b) GROUP_CONCAT default comma per team (rows concatenate in scan
    //    order); the SEPARATOR form restyles the roll call.
    let got = cells(rows(&mut c, "SELECT team, GROUP_CONCAT(name) FROM members GROUP BY team ORDER BY team").await);
    assert_eq!(got, vec![vec!["blue", "dee,eve,fang"], vec!["red", "ada,bob,cyd"]], "default comma");
    let got = cells(rows(&mut c, "SELECT team, GROUP_CONCAT(name SEPARATOR ' | ') FROM members GROUP BY team ORDER BY team").await);
    assert_eq!(got, vec![vec!["blue", "dee | eve | fang"], vec!["red", "ada | bob | cyd"]], "custom separator");

    // c) DISTINCT dedups within each group: red's double lead collapses.
    let got = cells(rows(&mut c, "SELECT team, GROUP_CONCAT(DISTINCT role) FROM members GROUP BY team ORDER BY team").await);
    assert_eq!(
        got,
        vec![vec!["blue", "lead,crew"], vec!["red", "lead,crew"]],
        "distinct roles, NULL skipped"
    );

    // d) The multi-arg form interleaves literals: id=role per member.
    assert_eq!(
        cell(&one(&mut c, "SELECT GROUP_CONCAT(name, '=', role) FROM members WHERE team = 'red'").await),
        "ada=lead,bob=crew,cyd=lead",
        "multi-arg concat"
    );

    // e) CONCAT_WS skips NULL cells (separator never propagates NULL);
    //    plain CONCAT over a NULL is NULL.
    let got = cells(rows(&mut c, "SELECT CONCAT_WS('-', name, role) FROM members WHERE team = 'blue' ORDER BY id").await);
    assert_eq!(
        got,
        vec![vec!["dee-lead"], vec!["eve"], vec!["fang-crew"]],
        "concat_ws skips null"
    );
    let got = cells(rows(&mut c, "SELECT CONCAT(name, '/', role) FROM members WHERE id = 5").await);
    assert_eq!(got, vec![vec!["<NULL>"]], "concat propagates null");

    // f) REGEXP is byte-wise and case-sensitive: lowercase '^a' hits
    //    ada only, the anchored class never hits uppercase spellings.
    let got = cells(rows(&mut c, "SELECT name FROM members WHERE name REGEXP '^a'").await);
    assert_eq!(got, vec![vec!["ada"]], "regexp case-sensitive");
    let got = cells(rows(&mut c, "SELECT name FROM members WHERE name REGEXP '^[a-c]'").await);
    assert_eq!(got, vec![vec!["ada"], vec!["bob"], vec!["cyd"]], "anchored class");
}
