//! Multi-store scenario #77 -- a course-enrollment roster on the SQL
//! plane: enrollments carry a COMPOSITE primary key (course, student)
//! so a student may sit many courses, INNER JOIN ... USING (student)
//! fattens rows from the student directory, LEFT JOIN keeps the
//! un-enrolled student visible with a NULL leg, ORDER BY + LIMIT pick
//! the honor roll, and COUNT/SUM aggregates over GROUP BY course total
//! the class book. One REAL node, MySQL front.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, rows, run};
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

/// Render one SQL cell: NULL keeps its own spelling so LEFT JOIN gaps
/// stay distinguishable from empty strings.
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        MVal::Double(d) => d.to_string(),
        other => format!("{other:?}"),
    }
}

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply.iter().map(|r| r.iter().map(cell).collect()).collect()
}

#[tokio::test]
async fn enrollment_roster_joins_pages_and_aggregates() {
    let _serial = scenario_common::serial_guard().await;
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, _a) = front_world("enroll77", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The student directory and the enrollment fact table; the
    //    composite PRIMARY KEY (course, student) admits re-enrollment
    //    of the same student across courses.
    ddl(
        &mut c,
        "CREATE TABLE students (student VARCHAR(16) PRIMARY KEY, name VARCHAR(32) NOT NULL)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE enrollments (course VARCHAR(16) NOT NULL, \
         student VARCHAR(16) NOT NULL, score INT NOT NULL, \
         PRIMARY KEY (course, student))",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO students (student, name) VALUES \
         ('s1', 'Ada'), ('s2', 'Grace'), ('s3', 'Edsger'), ('s4', 'Margaret'), ('s5', 'Alan')",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO enrollments (course, student, score) VALUES \
         ('db101', 's1', 90), ('db101', 's2', 85), ('db101', 's3', 70), \
         ('ai202', 's2', 95), ('ai202', 's4', 60)",
    )
    .await;

    // b) INNER JOIN with USING: every db101 row fattens with its name,
    //    ordered by score DESC.
    let roster = rows(
        &mut c,
        "SELECT e.student, s.name, e.score FROM enrollments e \
         JOIN students s USING (student) WHERE e.course = 'db101' ORDER BY e.score DESC",
    )
    .await;
    assert_eq!(
        grid(&roster),
        vec![
            vec!["s1", "Ada", "90"],
            vec!["s2", "Grace", "85"],
            vec!["s3", "Edsger", "70"]
        ],
        "inner join using"
    );

    // c) LEFT JOIN on a compound condition: the ai202 book keeps every
    //    student, the un-enrolled see a NULL score leg.
    let book = rows(
        &mut c,
        "SELECT st.student, e.score FROM students st \
         LEFT JOIN enrollments e ON e.student = st.student AND e.course = 'ai202' \
         ORDER BY st.student",
    )
    .await;
    assert_eq!(
        grid(&book),
        vec![
            vec!["s1", "<NULL>"],
            vec!["s2", "95"],
            vec!["s3", "<NULL>"],
            vec!["s4", "60"],
            vec!["s5", "<NULL>"],
        ],
        "left join keeps the absent"
    );

    // d) The strictly un-enrolled: LEFT JOIN ... IS NULL names Alan.
    let idle = rows(
        &mut c,
        "SELECT st.name FROM students st \
         LEFT JOIN enrollments e ON e.student = st.student WHERE e.student IS NULL",
    )
    .await;
    assert_eq!(grid(&idle), vec![vec!["Alan"]], "anti-join");

    // e) ORDER BY + LIMIT: the db101 honor roll is the top two.
    let top = rows(
        &mut c,
        "SELECT student, score FROM enrollments WHERE course = 'db101' \
         ORDER BY score DESC LIMIT 2",
    )
    .await;
    assert_eq!(
        grid(&top),
        vec![vec!["s1", "90"], vec!["s2", "85"]],
        "top-2"
    );

    // f) Aggregates: per-course COUNT and SUM total the class book.
    let totals = rows(
        &mut c,
        "SELECT course, COUNT(*) AS heads, SUM(score) AS total \
         FROM enrollments GROUP BY course ORDER BY course",
    )
    .await;
    assert_eq!(
        grid(&totals),
        vec![vec!["ai202", "2", "155"], vec!["db101", "3", "245"]],
        "group-by totals"
    );
    let grand = rows(&mut c, "SELECT COUNT(*), SUM(score) FROM enrollments").await;
    assert_eq!(grid(&grand), vec![vec!["5", "400"]], "grand totals");
}
