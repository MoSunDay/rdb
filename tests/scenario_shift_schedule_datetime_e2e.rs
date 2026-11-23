//! Multi-store scenario #104 -- the clinic shift planner on the SQL
//! plane: DATE_ADD walks a shift across INTERVAL units (DAY, the
//! month-end/leap clamp, HOUR promoting a bare date to a midnight
//! DATETIME, MINUTE on the clock column), DATEDIFF counts the nights
//! from the epoch anchor, and DATE_FORMAT renders both fixed-date
//! literals (cribbed exactly from the funcs suite -- no NOW()-style
//! nondeterminism) and the shift's own clock. One REAL node, MySQL
//! front.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, rows, run};
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
async fn shift_planner_walks_intervals_and_formats_clocks() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("shift104", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Two shifts: a January day starting 03:04:05, and a
    //    month-end (leap-year February) 8am start.
    ddl(&mut c, "CREATE TABLE shifts (id INT PRIMARY KEY, day DATE NOT NULL, \
        starts_at DATETIME NOT NULL)").await;
    run(&mut c, "INSERT INTO shifts (id, day, starts_at) VALUES \
        (1, '2024-01-02', '2024-01-02 03:04:05'), (2, '2024-01-31', '2024-03-10 08:00:00')").await;

    // b) DAY walk + the month clamp: 2024-01-31 + 1 MONTH clamps to
    //    the leap February 29th (crib).
    let got = rows(
        &mut c,
        "SELECT DATE_ADD(day, INTERVAL 3 DAY), DATE_ADD(day, INTERVAL 1 MONTH) \
         FROM shifts WHERE id = 2",
    )
    .await;
    assert_eq!(
        grid(&got),
        vec![vec!["2024-02-03", "2024-02-29"]],
        "day walk + month/leap clamp"
    );

    // c) HOUR promotes a bare DATE to a midnight DATETIME (25 hours
    //    crosses into the next day); MINUTE rides the clock column.
    let got = rows(
        &mut c,
        "SELECT DATE_ADD(day, INTERVAL 25 HOUR), \
         DATE_ADD(starts_at, INTERVAL 90 MINUTE) FROM shifts WHERE id = 1",
    )
    .await;
    assert_eq!(
        grid(&got),
        vec![vec!["2024-01-03 01:00:00", "2024-01-02 04:34:05"]],
        "hour promotion + minute walk"
    );

    // d) DATEDIFF: nights from the year anchor (date-only arithmetic,
    //    the clock does not matter).
    let got = rows(
        &mut c,
        "SELECT DATEDIFF(day, '2024-01-01'), DATEDIFF('2024-01-01', day) \
         FROM shifts WHERE id = 1",
    )
    .await;
    assert_eq!(grid(&got), vec![vec!["1", "-1"]], "datediff both directions");

    // e) DATE_FORMAT on the shift's own clock (%H:%i:%s crib) and on
    //    fixed-date literals (exact crib values, no NOW() involved).
    let got = rows(
        &mut c,
        "SELECT DATE_FORMAT(starts_at, '%H:%i:%s'), DATE_FORMAT(day, '%Y-%m-%d') \
         FROM shifts WHERE id = 1",
    )
    .await;
    assert_eq!(grid(&got), vec![vec!["03:04:05", "2024-01-02"]], "format the shift clock");
    for (sql, want) in [
        ("SELECT DATE_FORMAT('2024-02-29 13:05:09', '%Y-%m-%d')", "2024-02-29"),
        ("SELECT DATE_FORMAT('2024-02-29 13:05:09', '%T')", "13:05:09"),
        ("SELECT DATE_FORMAT('1970-01-01', '%W')", "Thursday"),
        ("SELECT DATE_FORMAT('2024-02-29 13:05:09', '%M')", "February"),
        ("SELECT DATE_FORMAT('2024-02-29 13:05:09', '%j')", "060"),
        ("SELECT DATE_FORMAT('2024-02-29 13:05:09', '%%')", "%"),
    ] {
        let got = rows(&mut c, sql).await;
        assert_eq!(grid(&got), vec![vec![want]], "{sql}");
    }
}
