//! Multi-store scenario #117 -- a two-ledger bank reconciliation on the
//! SQL plane, archived to S3: `ledger_a` and `ledger_b` share three
//! ids and each keeps two of its own, so a FULL OUTER JOIN is the only
//! join that reports the whole story (matched rows plus BOTH unmatched
//! sides, null-extended). Pinned: the exact 7-row join result, the
//! `USING (id)` spelling (both sides expand, the id column twice), the
//! anti-join (`WHERE b.id IS NULL` -> exactly the two left-only rows),
//! INTERSECT (the matched set) and EXCEPT (the unmatched set), and an
//! S3 round trip of the reconciliation report -- PUT then GET must be
//! byte-exact. One REAL node, MySQL + S3.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, run, server_error, ER_BAD_FIELD_ERROR};
use scenario_common::{front_world, s3_call, FrontSpec};
use std::time::Duration;

/// The fixture's pre-declared bucket (the S3 front ships with `rdb`).
const BUCKET: &str = "rdb";

/// `(id, amount)` of ledger A: five rows, three of them shared with B.
const A: [(i64, i64); 5] = [(1, 100), (2, 200), (3, 300), (4, 400), (5, 500)];
/// `(id, amount)` of ledger B: five rows, ids 6 and 7 exist only here.
const B: [(i64, i64); 5] = [(2, 220), (3, 300), (4, 410), (6, 600), (7, 700)];

/// The expected FULL OUTER result as sorted text rows (order-free
/// comparison: NULL cells spell `NULL`).
fn expected_full() -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = A
        .iter()
        .map(|(id, amount)| {
            let right = B.iter().find(|(bid, _)| bid == id);
            match right {
                Some((_, bamount)) => vec![
                    id.to_string(),
                    amount.to_string(),
                    id.to_string(),
                    bamount.to_string(),
                ],
                None => vec![
                    id.to_string(),
                    amount.to_string(),
                    "NULL".into(),
                    "NULL".into(),
                ],
            }
        })
        .collect();
    for (id, amount) in B
        .iter()
        .filter(|(id, _)| !A.iter().any(|(aid, _)| aid == id))
    {
        rows.push(vec![
            "NULL".into(),
            "NULL".into(),
            id.to_string(),
            amount.to_string(),
        ]);
    }
    sort_rows(rows)
}

/// Text rows of a query, in REPLY order (order-free comparisons call
/// `sort_rows` on the result).
async fn text_rows(c: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<String>> {
    common::mysql::rows(c, sql)
        .await
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| match v {
                    mysql_async::Value::NULL => "NULL".to_string(),
                    mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
                    other => format!("{other:?}"),
                })
                .collect()
        })
        .collect()
}

/// Deterministic row order for set comparisons.
fn sort_rows(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    rows.sort();
    rows
}

/// The three join shapes of the sweep (7 rows, 7 rows, 2 rows).
const JOINS: [&str; 3] = [
    "SELECT a.id, a.amount, b.id, b.amount FROM ledger_a a FULL OUTER JOIN \
     ledger_b b ON a.id = b.id",
    "SELECT * FROM ledger_a a FULL OUTER JOIN ledger_b b USING (id)",
    "SELECT a.id, a.amount FROM ledger_a a FULL OUTER JOIN ledger_b b ON a.id = b.id \
     WHERE b.id IS NULL",
];

/// Row counts seen by ONE connection: `per_conn` join queries cycling
/// over `JOINS`.
async fn sweep_one(
    mut c: mysql_async::Conn,
    joins: &[&'static str],
    per_conn: usize,
) -> Vec<usize> {
    let mut out = Vec::with_capacity(per_conn);
    for i in 0..per_conn {
        out.push(
            common::mysql::rows(&mut c, joins[i % joins.len()])
                .await
                .len(),
        );
    }
    out
}

/// One `id|a_amount|b_amount` line of the S3 reconciliation report.
fn report_line(id: &str, a_amount: &str, b_amount: &str) -> String {
    format!("{id}|{a_amount}|{b_amount}\n")
}

#[tokio::test]
async fn full_outer_reconciles_both_ledgers() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        s3: true,
        ..Default::default()
    };
    let (front, _resp) = front_world("recon117", &spec).await;
    let s3 = front.s3.clone();
    let mut c = connect_root(&front.node).await;

    // a) Both ledgers: same shape, overlapping id sets.
    ddl(
        &mut c,
        "CREATE TABLE ledger_a (id BIGINT PRIMARY KEY, amount INT NOT NULL)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE ledger_b (id BIGINT PRIMARY KEY, amount INT NOT NULL)",
    )
    .await;
    for (id, amount) in A {
        run(
            &mut c,
            &format!("INSERT INTO ledger_a VALUES ({id}, {amount})"),
        )
        .await;
    }
    for (id, amount) in B {
        run(
            &mut c,
            &format!("INSERT INTO ledger_b VALUES ({id}, {amount})"),
        )
        .await;
    }

    // b) `ORDER BY id` is ambiguous over a join that exposes TWO id
    //    columns -- standard SQL. Real behavior: MySQL 1054
    //    (ER_BAD_FIELD_ERROR) with an "ambiguous column" message, not a
    //    1064 parse error; the queries below qualify the column.
    let e = server_error(
        &mut c,
        "SELECT a.id, b.id FROM ledger_a a FULL OUTER JOIN ledger_b b ON a.id = b.id ORDER BY id",
    )
    .await;
    assert_eq!(
        e.code, ER_BAD_FIELD_ERROR,
        "ambiguous ORDER BY id: {}",
        e.message
    );
    assert!(
        e.message.contains("ambiguous column"),
        "ambiguity message: {}",
        e.message
    );

    // c) The hot block: 200 join queries (three shapes) + 3 set ops.
    //    One statement costs ~40ms of round trip whatever it says, so
    //    the 200 joins ride 10 concurrent connections (20 each).
    let t = scenario_common::perf::now();
    let mut conns = Vec::new();
    for _ in 0..10 {
        conns.push(connect_root(&front.node).await);
    }
    let mut handles = Vec::new();
    for c in conns {
        handles.push(tokio::spawn(sweep_one(c, &JOINS, 20)));
    }
    let mut counts = Vec::new();
    for h in handles {
        counts.extend(h.await.expect("join sweep task"));
    }
    assert_eq!(counts.len(), 200, "200 join queries ran");
    assert_eq!(
        counts.iter().filter(|c| **c == 7).count(),
        140,
        "{counts:?}"
    );
    assert_eq!(counts.iter().filter(|c| **c == 2).count(), 60, "{counts:?}");
    for sql in [
        "SELECT id FROM ledger_a INTERSECT SELECT id FROM ledger_b",
        "SELECT id FROM ledger_a EXCEPT SELECT id FROM ledger_b",
        "SELECT id FROM ledger_b EXCEPT SELECT id FROM ledger_a",
    ] {
        common::mysql::rows(&mut c, sql).await;
    }
    let elapsed = scenario_common::perf::within(
        "recon117/200-join+3-setop",
        t,
        Duration::from_secs(2),
    );
    scenario_common::perf::rate("recon117/200-join+3-setop", 203, elapsed, 50.0);

    // d) The exact FULL OUTER row list: matched rows keep both sides,
    //    left-only rows null-extend B, right-only rows null-extend A.
    let got = text_rows(
        &mut c,
        "SELECT a.id, a.amount, b.id, b.amount FROM ledger_a a FULL OUTER JOIN \
         ledger_b b ON a.id = b.id ORDER BY a.id, b.id",
    )
    .await;
    assert_eq!(got.len(), 7, "full outer row count: {got:?}");
    assert_eq!(sort_rows(got.clone()), expected_full(), "full outer rows");
    // the a-side rows answer in ascending id order (the two b-only rows
    // sort separately because their a.id is NULL)
    assert_eq!(
        text_rows(
            &mut c,
            "SELECT a.id, a.amount FROM ledger_a a FULL OUTER JOIN ledger_b b ON a.id = b.id \
             WHERE a.id IS NOT NULL ORDER BY a.id"
        )
        .await,
        vec![
            vec!["1", "100"],
            vec!["2", "200"],
            vec!["3", "300"],
            vec!["4", "400"],
            vec!["5", "500"]
        ],
        "a-side order"
    );

    // e) `USING (id)`: both sides expand -- the id column appears
    //    twice, so the unmatched rows are still readable on each side.
    let using = text_rows(
        &mut c,
        "SELECT * FROM ledger_a a FULL OUTER JOIN ledger_b b USING (id)",
    )
    .await;
    assert_eq!(using.len(), 7, "using row count: {using:?}");
    assert_eq!(sort_rows(using.clone()), expected_full(), "USING (id) rows");
    for row in &using {
        assert_eq!(row.len(), 4, "both sides expand: {row:?}");
    }

    // f) Anti-join: exactly the two left-only rows (ids 1 and 5).
    let anti = text_rows(
        &mut c,
        "SELECT a.id, a.amount FROM ledger_a a FULL OUTER JOIN ledger_b b ON a.id = b.id \
         WHERE b.id IS NULL ORDER BY a.id",
    )
    .await;
    assert_eq!(
        anti,
        vec![vec!["1", "100"], vec!["5", "500"]],
        "anti-join: {anti:?}"
    );
    // ... and the mirror anti-join on the other side: ids 6 and 7.
    let anti_b = text_rows(
        &mut c,
        "SELECT b.id, b.amount FROM ledger_a a FULL OUTER JOIN ledger_b b ON a.id = b.id \
         WHERE a.id IS NULL ORDER BY b.id",
    )
    .await;
    assert_eq!(
        anti_b,
        vec![vec!["6", "600"], vec!["7", "700"]],
        "right anti-join: {anti_b:?}"
    );

    // g) INTERSECT is the matched set, EXCEPT each side's unmatched set.
    assert_eq!(
        sort_rows(
            text_rows(
                &mut c,
                "SELECT id FROM ledger_a INTERSECT SELECT id FROM ledger_b"
            )
            .await
        ),
        vec![vec!["2"], vec!["3"], vec!["4"]],
        "intersect"
    );
    assert_eq!(
        sort_rows(
            text_rows(
                &mut c,
                "SELECT id FROM ledger_a EXCEPT SELECT id FROM ledger_b"
            )
            .await
        ),
        vec![vec!["1"], vec!["5"]],
        "except a-b"
    );
    assert_eq!(
        sort_rows(
            text_rows(
                &mut c,
                "SELECT id FROM ledger_b EXCEPT SELECT id FROM ledger_a"
            )
            .await
        ),
        vec![vec!["6"], vec!["7"]],
        "except b-a"
    );
    // matched ids == the intersection == the join's inner core.
    assert_eq!(
        text_rows(
            &mut c,
            "SELECT a.id FROM ledger_a a JOIN ledger_b b ON a.id = b.id ORDER BY a.id"
        )
        .await,
        vec![vec!["2"], vec!["3"], vec!["4"]],
        "inner core"
    );

    // h) S3 archive: the reconciliation report is a deterministic byte
    //    body; GET must return it byte-exact.
    let mut report = String::from("id|ledger_a|ledger_b\n");
    for row in expected_full() {
        report.push_str(&report_line(&row[0], &row[1], &row[3]));
    }
    let body = report.as_bytes().to_vec();
    let (status, head, _) = s3_call(
        &s3,
        "PUT",
        &format!("/{BUCKET}/recon/full-outer.txt"),
        &body,
    )
    .await;
    assert_eq!(status, 200, "put report: {head}");
    let (status, head, got) =
        s3_call(&s3, "GET", &format!("/{BUCKET}/recon/full-outer.txt"), b"").await;
    assert_eq!(status, 200, "get report: {head}");
    assert_eq!(got, body, "byte-exact report");
    // the archived matched set is the intersection, spelled the same way
    let matched = "id\n2\n3\n4\n".as_bytes().to_vec();
    let (status, head, _) = s3_call(
        &s3,
        "PUT",
        &format!("/{BUCKET}/recon/matched.txt"),
        &matched,
    )
    .await;
    assert_eq!(status, 200, "put matched: {head}");
    let (status, _, got) = s3_call(&s3, "GET", &format!("/{BUCKET}/recon/matched.txt"), b"").await;
    assert_eq!(status, 200, "get matched");
    assert_eq!(got, matched, "byte-exact matched set");
    scenario_common::perf::scenario("recon117", t0);
}
