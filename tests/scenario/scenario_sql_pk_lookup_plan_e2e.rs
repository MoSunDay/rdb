//! Multi-store scenario #170 -- a quarantined-sellers admin tool previews each
//! lookup against the planner. EXPLAIN = ONE `plan` column, one row per line:
//! whole-PK `=` / deduped pk IN -> `PkLookup sellers -> N pks`; the FIRST
//! sargable (=, IN, BETWEEN) conjunct in TEXT order with an index ->
//! `IndexScan <ix> -> N pks` (no statistics); else `SeqScan sellers` +
//! `Filter:`; `Order:` / `Limit: n Offset: m`; residual filter re-applied.
//! EXPLAIN INSERT/DELETE/DDL -> `Direct execution (<kind>)`, not executed.
//! SHOW INDEX: 6 columns, PRIMARY first, creation order; INDEXES/KEYS 1235.
//! A RESP ZSET (score = priority) drives `id IN (...)`. One REAL node, MySQL
//! front + RESP. Deviation: OR over the pk stays SeqScan after CREATE INDEX
//! ix_id (src/sql/plan/mod.rs collect_conjuncts); `>` is not sargable.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{col, connect_root, ddl, ints, one, rows, server_error};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, FrontSpec};

const ER_NOT_SUPPORTED_YET: u16 = 1235;

/// One seller: (id, region, amount, status).
type Seller = (i64, &'static str, i64, i64);

/// One EXPLAIN ladder case: WHERE text, its row predicate, plan headline.
type Case = (&'static str, fn(&Seller) -> bool, String);

/// The six sellers under review ('eu' = ids 1, 3, 4; the amounts cross the
/// residual thresholds 70, 100 and 500).
const SELLERS: [Seller; 6] = [
    (1, "eu", 120, 1),
    (2, "us", 80, 1),
    (3, "eu", 60, 2),
    (4, "eu", 300, 1),
    (5, "apac", 450, 2),
    (6, "us", 200, 1),
];

fn cell(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::Int(i) => i.to_string(),
        MVal::NULL => "NULL".to_string(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn grid(reply: &[Vec<MVal>]) -> Vec<Vec<String>> {
    reply.iter().map(|r| r.iter().map(cell).collect()).collect()
}

/// Ids of the sellers matching `pred`, ascending.
fn pick(pred: fn(&Seller) -> bool) -> Vec<i64> {
    let mut ids: Vec<i64> = SELLERS.iter().filter(|s| pred(s)).map(|s| s.0).collect();
    ids.sort_unstable();
    ids
}

/// Distinct members of an IN list (the planner dedupes before counting).
fn distinct(list: &[i64]) -> usize {
    list.iter().collect::<BTreeSet<_>>().len()
}

/// Column names of a result set (first row's metadata).
async fn col_names(c: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    let rs: Vec<mysql_async::Row> = c.query(sql).await.expect(sql);
    rs.first()
        .map(|r| r.columns_ref().iter().map(|k| k.name_str().to_string()).collect())
        .unwrap_or_default()
}

/// Plan lines of `EXPLAIN <stmt>` (the single `plan` cell of every row).
async fn plan(c: &mut mysql_async::Conn, stmt: &str) -> Vec<String> {
    col(c, &format!("EXPLAIN {stmt}"))
        .await
        .iter()
        .map(cell)
        .collect()
}

/// Members of a ZSET in ZRANGE (score) order: the bulk payloads of the
/// array, read through the quiet-gap drain (arrays need the full reply).
async fn zrange(addr: &str, key: &str) -> Vec<String> {
    let args: Vec<&[u8]> = vec![b"ZRANGE", key.as_bytes(), b"0", b"-1"];
    let full = common::lite::text(
        &common::lite::cmd_full_reply_wait(addr, common::TOKEN, &args, 25, 10_000).await,
    );
    full.split("\r\n")
        .collect::<Vec<&str>>()
        .windows(2)
        .filter(|w| w[0].starts_with('$') && w[0] != "$-1")
        .map(|w| w[1].to_string())
        .collect()
}

#[tokio::test]
async fn sql_pk_lookup_plan() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, resp) = front_world("pkplan170", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) The sellers table, two secondary indexes, six rows in ONE
    //    multi-row INSERT (no index on the pk yet).
    ddl(
        &mut c,
        "CREATE TABLE sellers (id INT PRIMARY KEY, region VARCHAR(32), amount INT, status INT)",
    )
    .await;
    ddl(&mut c, "CREATE INDEX ix_region ON sellers(region)").await;
    ddl(&mut c, "CREATE INDEX ix_amount ON sellers(amount)").await;
    let values: Vec<String> = SELLERS
        .iter()
        .map(|(i, rg, a, st)| format!("({i}, '{rg}', {a}, {st})"))
        .collect();
    let inserted = common::mysql::aff(
        &mut c,
        &format!(
            "INSERT INTO sellers (id, region, amount, status) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
    assert_eq!(inserted as usize, SELLERS.len(), "multi-row INSERT affected = rows");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM sellers").await, vec![SELLERS.len() as i64]);

    // b) Hot block: the EXPLAIN ladder -- every headline, the full WHERE
    //    rendered as `Filter:`, the projection -- and the equality queries
    //    (the lookup plan's rows == the Rust expectation).
    let t = scenario_common::perf::now();
    let eu = pick(|s| s.1 == "eu").len();
    let wide = pick(|s| (100..=500).contains(&s.2)).len();
    let in_list = [3, 1, 3, 2];
    let ladder: Vec<Case> = vec![
        ("id = 4 AND region = 'eu'", |s| s.0 == 4 && s.1 == "eu", "PkLookup sellers -> 1 pks".to_string()),
        ("id IN (3, 1, 3, 2)", |s| s.0 == 1 || s.0 == 2 || s.0 == 3, format!("PkLookup sellers -> {} pks", distinct(&in_list))),
        ("region = 'eu' AND amount > 100", |s| s.1 == "eu" && s.2 > 100, format!("IndexScan ix_region -> {eu} pks")),
        ("amount > 100 AND region = 'eu'", |s| s.1 == "eu" && s.2 > 100, format!("IndexScan ix_region -> {eu} pks")),
        ("region = 'eu' AND amount BETWEEN 100 AND 500", |s| s.1 == "eu" && (100..=500).contains(&s.2), format!("IndexScan ix_region -> {eu} pks")),
        ("amount BETWEEN 100 AND 500 AND region = 'eu'", |s| s.1 == "eu" && (100..=500).contains(&s.2), format!("IndexScan ix_amount -> {wide} pks")),
        ("id BETWEEN 1 AND 3", |s| (1..=3).contains(&s.0), "SeqScan sellers".to_string()),
        ("id = 1 OR id = 2", |s| s.0 == 1 || s.0 == 2, "SeqScan sellers".to_string()),
        ("id IN (1, 2, 3) AND amount > 70", |s| (1..=3).contains(&s.0) && s.2 > 70, "PkLookup sellers -> 3 pks".to_string()),
    ];
    let shape = col_names(&mut c, "EXPLAIN SELECT id FROM sellers").await;
    assert_eq!(shape, vec!["plan".to_string()], "EXPLAIN is ONE column named plan");
    for (w, pred, head) in &ladder {
        let stmt = format!("SELECT id FROM sellers WHERE {w}");
        let lines = plan(&mut c, &stmt).await;
        assert_eq!(lines[0], *head, "headline of `{w}`: {lines:?}");
        assert_eq!(lines[1], format!("Filter: {w}"), "full WHERE after the headline: {lines:?}");
        assert_eq!(lines[2], "Project: id", "{lines:?}");
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(ints(&mut c, &format!("{stmt} ORDER BY id")).await, pick(*pred), "rows of `{w}`");
    }
    // The residual filter after the pk lookup really filtered: the IN list
    // resolved 3 pks, only the two amounts above 70 survived.
    assert_eq!(pick(|s| (1..=3).contains(&s.0) && s.2 > 70), vec![1, 2]);
    // An equivalent SeqScan formulation (CAST(...) hides the column from the
    // sargable recognizer) answers the same rows as the IndexScan plan.
    let scan_form = "SELECT id FROM sellers WHERE CAST(region AS CHAR) = 'eu' \
                     AND amount > 100 ORDER BY id";
    assert_eq!(plan(&mut c, scan_form).await[0], "SeqScan sellers", "non-sargable -> SeqScan");
    assert_eq!(ints(&mut c, scan_form).await, pick(|s| s.1 == "eu" && s.2 > 100));
    // ORDER BY / LIMIT render exactly as written (offset 1, 2 rows of the
    // amount-descending ranking).
    let mut ranked: Vec<Seller> = SELLERS.to_vec();
    ranked.sort_by_key(|s| std::cmp::Reverse(s.2));
    let window: Vec<i64> = ranked.iter().skip(1).take(2).map(|s| s.0).collect();
    let top = "SELECT id FROM sellers ORDER BY amount DESC LIMIT 2 OFFSET 1";
    assert_eq!(
        plan(&mut c, top).await,
        vec!["SeqScan sellers", "Project: id", "Order: amount DESC", "Limit: 2 Offset: 1"]
    );
    assert_eq!(ints(&mut c, top).await, window, "LIMIT 2 OFFSET 1 window");
    scenario_common::perf::within("pkplan/explain-ladder", t, Duration::from_secs(3));
    // CREATE INDEX ix_id: BETWEEN over the pk becomes an IndexScan, the OR
    // stays SeqScan (deviation: a top-level OR is one opaque conjunct).
    let t = scenario_common::perf::now();
    ddl(&mut c, "CREATE INDEX ix_id ON sellers(id)").await;
    let between = "SELECT id FROM sellers WHERE id BETWEEN 1 AND 3 ORDER BY id";
    let low3 = pick(|s| (1..=3).contains(&s.0));
    assert_eq!(plan(&mut c, between).await[0], format!("IndexScan ix_id -> {} pks", low3.len()));
    assert_eq!(ints(&mut c, between).await, low3);
    let or = "SELECT id FROM sellers WHERE id = 1 OR id = 2 ORDER BY id";
    assert_eq!(plan(&mut c, or).await[0], "SeqScan sellers", "OR is never sargable");
    assert_eq!(ints(&mut c, or).await, vec![1, 2]);
    scenario_common::perf::within("pkplan/after-ix-id", t, Duration::from_secs(2));

    // c) EXPLAIN of a write or DDL is one `Direct execution (<kind>)` row
    //    and does NOT execute: the count and the index list stay as they were.
    let count_before = one(&mut c, "SELECT COUNT(*) FROM sellers").await;
    let index_names: Vec<String> = rows(&mut c, "SHOW INDEX FROM sellers")
        .await
        .iter()
        .map(|r| cell(&r[2]))
        .collect();
    for (stmt, kind) in [
        ("INSERT INTO sellers VALUES (99,'x',1,1)", "insert"),
        ("DELETE FROM sellers WHERE id = 1", "delete"),
        ("CREATE INDEX ix_tmp ON sellers(status)", "ddl"),
    ] {
        assert_eq!(plan(&mut c, stmt).await, vec![format!("Direct execution ({kind})")], "{stmt}");
    }
    assert_eq!(one(&mut c, "SELECT COUNT(*) FROM sellers").await, count_before, "not executed");
    assert_eq!(
        rows(&mut c, "SHOW INDEX FROM sellers").await.iter().map(|r| cell(&r[2])).collect::<Vec<_>>(),
        index_names,
        "no ix_tmp was created"
    );

    // d) SHOW INDEX: exactly the 6 MySQL columns in order, PRIMARY first
    //    (Non_unique 0), then the indexes in creation order; the
    //    SHOW INDEXES / SHOW KEYS spellings are 1235.
    let idx_cols = col_names(&mut c, "SHOW INDEX FROM sellers").await;
    assert_eq!(
        idx_cols,
        ["Table", "Non_unique", "Key_name", "Seq_in_index", "Column_name", "Index_type"]
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>(),
        "SHOW INDEX column names"
    );
    let row = |ix: &str, nu: &str, colname: &str| -> Vec<String> {
        vec!["sellers".into(), nu.into(), ix.into(), "1".into(), colname.into(), "BTREE".into()]
    };
    assert_eq!(
        grid(&rows(&mut c, "SHOW INDEX FROM sellers").await),
        vec![
            row("PRIMARY", "0", "id"),
            row("ix_region", "1", "region"),
            row("ix_amount", "1", "amount"),
            row("ix_id", "1", "id")
        ],
        "PRIMARY first, then creation order"
    );
    for sql in ["SHOW INDEXES FROM sellers", "SHOW KEYS FROM sellers"] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
        assert!(e.message.contains("not supported"), "{sql}: {e:?}");
    }

    // e) The RESP quarantine list drives the lookup: ZADD order is
    //    unsorted text, ZRANGE answers by score (1, 2, 3), and the IN list
    //    built from that order plans as a deduped PkLookup.
    let zadd = r(&resp, &[b"ZADD", b"quarantine", b"3", b"3", b"1", b"1", b"2", b"2"]).await;
    assert_eq!(int_of(&zadd), 3, "three new members: {zadd:?}");
    let members = zrange(&resp, "quarantine").await;
    assert_eq!(members, vec!["1", "2", "3"], "ZRANGE order = priority");
    let lookup = format!("SELECT id, region, amount FROM sellers WHERE id IN ({}) ORDER BY id", members.join(", "));
    let want: Vec<Vec<String>> = SELLERS
        .iter()
        .filter(|s| members.contains(&s.0.to_string()))
        .map(|s| vec![s.0.to_string(), s.1.to_string(), s.2.to_string()])
        .collect();
    assert_eq!(grid(&rows(&mut c, &lookup).await), want, "quarantined sellers");
    assert_eq!(plan(&mut c, &lookup).await[0], format!("PkLookup sellers -> {} pks", members.len()));

    scenario_common::perf::scenario("pkplan", t0);
}
