//! Multi-store scenario #86 -- a grade-curve desk on the SQL plane:
//! CASE WHEN buckets raw points into letter grades per row (branching
//! plus ELSE, evaluated over table columns), GROUP BY rolls the buckets
//! up with COUNT/AVG while HAVING keeps only the fat buckets, ORDER BY
//! sorts on two columns at once (grade asc, then the student name as
//! tiebreak), and LIMIT n OFFSET m windows the ordered stream. One REAL
//! node, MySQL front.

mod common;
mod scenario_common;

use common::mysql::{connect_root, ddl, rows, run};
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
async fn grade_curve_buckets_groups_fences_and_windows() {
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("gc86", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Eight exam rows: 3 A's, 2 B's, 2 C's, 1 F.
    ddl(&mut c, "CREATE TABLE scores (id BIGINT PRIMARY KEY, student VARCHAR(16) NOT NULL, points INT NOT NULL)").await;
    run(&mut c, "INSERT INTO scores (id, student, points) VALUES \
        (1,'ivy',96),(2,'jack',91),(3,'kate',90),(4,'liam',85),\
        (5,'mona',82),(6,'noah',74),(7,'olga',70),(8,'pete',8)").await;

    // b) CASE WHEN over table columns buckets every row; the ELSE
    //    catches the failing score.
    let got = cells(rows(&mut c, "SELECT student, CASE WHEN points >= 90 THEN 'A' WHEN points >= 80 THEN 'B' WHEN points >= 60 THEN 'C' ELSE 'F' END AS grade FROM scores ORDER BY id").await);
    assert_eq!(
        got,
        vec![
            vec!["ivy", "A"], vec!["jack", "A"], vec!["kate", "A"],
            vec!["liam", "B"], vec!["mona", "B"],
            vec!["noah", "C"], vec!["olga", "C"],
            vec!["pete", "F"],
        ],
        "per-row buckets"
    );

    // c) GROUP BY the alias + HAVING: only buckets with >= 2 students
    //    survive (F drops out).
    let got = cells(rows(&mut c, "SELECT CASE WHEN points >= 90 THEN 'A' WHEN points >= 80 THEN 'B' WHEN points >= 60 THEN 'C' ELSE 'F' END AS grade, COUNT(*) AS n, AVG(points) AS avg_pts FROM scores GROUP BY grade HAVING COUNT(*) >= 2 ORDER BY grade").await);
    assert_eq!(
        got,
        vec![
            vec!["A", "3", "92.3333"], // (96+91+90)/3, scale-4 decimal
            vec!["B", "2", "83.5000"],            // (85+82)/2
            vec!["C", "2", "72.0000"],            // (74+70)/2
        ],
        "having fences the thin bucket"
    );

    // d) ORDER BY two columns at once: grade asc, student asc as the
    //    tiebreak (i-before-j-before-k inside A).
    let got = cells(rows(&mut c, "SELECT CASE WHEN points >= 90 THEN 'A' WHEN points >= 80 THEN 'B' WHEN points >= 60 THEN 'C' ELSE 'F' END AS grade, student FROM scores ORDER BY grade, student").await);
    assert_eq!(
        got,
        vec![
            vec!["A", "ivy"], vec!["A", "jack"], vec!["A", "kate"],
            vec!["B", "liam"], vec!["B", "mona"],
            vec!["C", "noah"], vec!["C", "olga"],
            vec!["F", "pete"],
        ],
        "two-column order"
    );

    // e) LIMIT n OFFSET m windows the ordered stream: page past the
    //    three A's, take the next two rows.
    let got = cells(rows(&mut c, "SELECT student FROM scores ORDER BY id LIMIT 2 OFFSET 3").await);
    assert_eq!(got, vec![vec!["liam"], vec!["mona"]], "offset window");
}
