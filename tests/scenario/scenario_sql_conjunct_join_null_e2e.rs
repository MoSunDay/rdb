//! Multi-store scenario #171 -- a customer census runs the SAME tables on
//! both engines of one node (host `customers`/`cities` + the turso library
//! `turso_census`). An INT-key INNER equi-join never matches a NULL key,
//! LEFT null-extends, RIGHT is implemented; conjunct order (2- and 3-term
//! chains incl. LENGTH) changes nothing; ORDER BY puts NULLs FIRST ascending
//! / LAST descending (host deviation vs MySQL: select.rs cmp_null_first;
//! turso = SQLite NULLs-smallest, the same); `NOT IN (1, NULL)` is EMPTY,
//! `IN (1, NULL)` keeps the city-1 rows; LEFT JOIN + COUNT(c.id) GROUP BY
//! counts 0 for the unmatched city. `IS NULL` / `IS NOT NULL` run on BOTH
//! engines (the turso renderer used to spell `x IS NULL` as `(x IS) NULL`
//! -> 1064; fixed by the batch-14 R2 closure, translate/expr.rs). `<=>`
//! is host-only by design: turso still rejects it with 1235.
//! One REAL node, MySQL front (host engine + turso engine).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{col, ints, one, rows, s};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

/// One customer: (id, name, city_id or NULL).
type Customer = (i64, &'static str, Option<i64>);

/// Six customers; `cy` and `fay` have no city (NULL key).
const CUSTOMERS: [Customer; 6] = [
    (1, "ann", Some(1)),
    (2, "bo", Some(2)),
    (3, "cy", None),
    (4, "di", Some(1)),
    (5, "ed", Some(2)),
    (6, "fay", None),
];

/// Three cities; `kyiv` has no customer.
const CITIES: [(i64, &str); 3] = [(1, "oslo"), (2, "lima"), (3, "kyiv")];

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

/// `SELECT c.id, c.name, t.id, t.name` of the census join in nested-loop
/// (left-major) order; `left_outer` null-extends a customer without a city.
fn join_rows(left_outer: bool) -> Vec<Vec<MVal>> {
    let mut out = Vec::new();
    for (cid, cname, city) in CUSTOMERS {
        let mut matched = false;
        for (tid, tname) in CITIES {
            if city == Some(tid) {
                matched = true;
                out.push(vec![common::mysql::int(cid), s(cname), common::mysql::int(tid), s(tname)]);
            }
        }
        if !matched && left_outer {
            out.push(vec![common::mysql::int(cid), s(cname), MVal::NULL, MVal::NULL]);
        }
    }
    out
}

/// The RIGHT join grouped by city (`ORDER BY t.id, c.id`): the matched
/// pairs in customer order, then the unmatched city null-extended leftwards.
fn right_join_rows() -> Vec<Vec<MVal>> {
    let mut out = Vec::new();
    for (tid, tname) in CITIES {
        let mut matched = false;
        for (cid, cname, city) in CUSTOMERS {
            if city == Some(tid) {
                matched = true;
                out.push(vec![common::mysql::int(cid), s(cname), common::mysql::int(tid), s(tname)]);
            }
        }
        if !matched {
            out.push(vec![MVal::NULL, MVal::NULL, common::mysql::int(tid), s(tname)]);
        }
    }
    out
}

/// `city_id` in ORDER BY order: NULLs are smallest (FIRST ascending, LAST
/// descending) -- the host rule and SQLite's.
fn city_order(desc: bool) -> Vec<MVal> {
    let mut v: Vec<Option<i64>> = CUSTOMERS.iter().map(|c| c.2).collect();
    v.sort();
    if desc {
        v.reverse();
    }
    v.into_iter()
        .map(|x| x.map(common::mysql::int).unwrap_or(MVal::NULL))
        .collect()
}

/// Ids of the customers in city `tid` matching `pred`, ascending.
fn in_city(tid: i64, pred: fn(&Customer) -> bool) -> Vec<i64> {
    let mut ids: Vec<i64> = CUSTOMERS
        .iter()
        .filter(|c| c.2 == Some(tid) && pred(c))
        .map(|c| c.0)
        .collect();
    ids.sort_unstable();
    ids
}

/// The census per city `(name, customers)`, sorted by name.
fn census() -> Vec<Vec<String>> {
    let mut v: Vec<(String, usize)> = CITIES
        .iter()
        .map(|(tid, tname)| {
            let n = CUSTOMERS.iter().filter(|c| c.2 == Some(*tid)).count();
            (tname.to_string(), n)
        })
        .collect();
    v.sort();
    v.into_iter().map(|(n, k)| vec![n, k.to_string()]).collect()
}

/// The census rows: one multi-row literal INSERT per table.
async fn seed(c: &mut mysql_async::Conn) {
    let cv: Vec<String> = CUSTOMERS
        .iter()
        .map(|(i, n, city)| {
            let city = city.map(|x| x.to_string()).unwrap_or_else(|| "NULL".to_string());
            format!("({i}, '{n}', {city})")
        })
        .collect();
    let n = common::mysql::aff(
        c,
        &format!("INSERT INTO customers (id, name, city_id) VALUES {}", cv.join(", ")),
    )
    .await;
    assert_eq!(n as usize, CUSTOMERS.len(), "customers inserted");
    let tv: Vec<String> = CITIES.iter().map(|(i, n)| format!("({i}, '{n}')")).collect();
    let n = common::mysql::aff(
        c,
        &format!("INSERT INTO cities (id, name) VALUES {}", tv.join(", ")),
    )
    .await;
    assert_eq!(n as usize, CITIES.len(), "cities inserted");
}

/// Engine-agnostic census reads: host and turso answer the SAME rows.
async fn census_reads(c: &mut mysql_async::Conn) {
    // 1) Joins: NULL keys never match; LEFT null-extends; RIGHT keeps
    //    every city (the unmatched one null-extended leftwards).
    let inner = "SELECT c.id, c.name, t.id, t.name FROM customers c JOIN cities t \
                 ON c.city_id = t.id ORDER BY c.id, t.id";
    assert_eq!(grid(&rows(c, inner).await), grid(&join_rows(false)), "INNER join");
    assert_eq!(
        grid(&rows(c, &inner.replace("JOIN", "LEFT JOIN")).await),
        grid(&join_rows(true)),
        "LEFT join null-extends"
    );
    let right = "SELECT c.id, c.name, t.id, t.name FROM customers c RIGHT JOIN cities t \
                 ON c.city_id = t.id ORDER BY t.id, c.id";
    assert_eq!(grid(&rows(c, right).await), grid(&right_join_rows()), "RIGHT join");
    assert_eq!(one(c, "SELECT COUNT(*) FROM customers c JOIN cities t ON c.city_id = t.id").await, s("4"));

    // 2) Conjunct order: reversed and three-term chains (LENGTH is not
    //    sargable) answer the same rows.
    let oslo = in_city(1, |_| true);
    let oslo_above_2 = in_city(1, |c| c.0 > 2);
    let oslo_long = in_city(1, |c| c.0 > 0 && c.1.len() > 2);
    for (w, want) in [
        ("t.name = 'oslo' AND c.id > 2", &oslo_above_2),
        ("c.id > 2 AND t.name = 'oslo'", &oslo_above_2),
        ("t.name = 'oslo' AND c.id > 0 AND LENGTH(c.name) > 2", &oslo_long),
        ("LENGTH(c.name) > 2 AND c.id > 0 AND t.name = 'oslo'", &oslo_long),
    ] {
        let got = ints(
            c,
            &format!("SELECT c.id FROM customers c JOIN cities t ON c.city_id = t.id WHERE {w} ORDER BY c.id"),
        )
        .await;
        assert_eq!(got, *want, "conjuncts `{w}`");
    }
    assert_eq!(oslo, vec![1, 4], "oslo customers");

    // 3) NULL ordering: FIRST ascending, LAST descending.
    assert_eq!(col(c, "SELECT city_id FROM customers ORDER BY city_id ASC").await, city_order(false), "ASC");
    assert_eq!(col(c, "SELECT city_id FROM customers ORDER BY city_id DESC").await, city_order(true), "DESC");

    // 4) Three-valued IN: NOT IN (1, NULL) is empty, IN (1, NULL) the
    //    city-1 rows.
    assert!(ints(c, "SELECT id FROM customers WHERE city_id NOT IN (1, NULL) ORDER BY id").await.is_empty());
    assert_eq!(ints(c, "SELECT id FROM customers WHERE city_id IN (1, NULL) ORDER BY id").await, oslo);

    // 5) The census: every city exactly once, COUNT(c.id) 0 for the
    //    unmatched city.
    let agg = "SELECT t.name, COUNT(c.id) FROM cities t LEFT JOIN customers c \
               ON c.city_id = t.id GROUP BY t.name ORDER BY t.name";
    assert_eq!(grid(&rows(c, agg).await), census(), "LEFT JOIN census");
}

/// NULL predicates, engine-agnostic since the turso renderer fix
/// (batch-14 R2: `(x IS) NULL` -> 1064): `IS NULL` as the residual
/// after an IN list, the `IS NULL` / `IS NOT NULL` id sets, and the
/// `IS NOT NULL` count. Host and turso answer the SAME rows.
async fn null_predicates(c: &mut mysql_async::Conn) {
    let nulls: Vec<i64> = (1..=3i64).filter(|i| CUSTOMERS[*i as usize - 1].2.is_none()).collect();
    let residual = ints(c, "SELECT id FROM customers WHERE id IN (1, 2, 3) AND city_id IS NULL ORDER BY id").await;
    assert_eq!(residual, nulls, "IN + IS NULL residual");
    let uncited: Vec<i64> = CUSTOMERS.iter().filter(|c| c.2.is_none()).map(|c| c.0).collect();
    let cited: Vec<i64> = CUSTOMERS.iter().filter(|c| c.2.is_some()).map(|c| c.0).collect();
    assert_eq!(ints(c, "SELECT id FROM customers WHERE city_id IS NULL ORDER BY id").await, uncited, "IS NULL ids");
    assert_eq!(
        ints(c, "SELECT id FROM customers WHERE city_id IS NOT NULL ORDER BY id").await,
        cited,
        "IS NOT NULL ids"
    );
    assert_eq!(ints(c, "SELECT COUNT(*) FROM customers WHERE city_id IS NOT NULL").await, vec![cited.len() as i64]);
    // Three-valued logic: `= NULL` is never TRUE on either engine.
    assert!(ints(c, "SELECT id FROM customers WHERE city_id = NULL").await.is_empty(), "= NULL is empty");
}

/// Host-only: the NULL-safe `<=>` counts the NULL-key rows the way
/// `IS NULL` does (turso rejects the operator with 1235 -- see the
/// library leg and turso_crud_e2e's matrix).
async fn null_safe_equal(c: &mut mysql_async::Conn) {
    let null_keys = CUSTOMERS.iter().filter(|c| c.2.is_none()).count();
    assert_eq!(ints(c, "SELECT COUNT(*) FROM customers WHERE city_id <=> NULL").await, vec![null_keys as i64]);
}

#[tokio::test]
async fn sql_conjunct_join_null() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    // The turso library root lives OUTSIDE the scenario world dir (which
    // `front_world` wipes first), so create it before spawning.
    let turso_dir = std::env::temp_dir().join(format!("rdb-scenario-turso171-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&turso_dir);
    std::fs::create_dir_all(&turso_dir).expect("create turso db dir");
    let spec = FrontSpec {
        mysql: true,
        extra_yaml: format!("turso_engine: true\nturso_db_dir: \"{}\"\n", turso_dir.display()),
        ..Default::default()
    };
    let (front, _resp) = front_world("conjnull171", &spec).await;
    let mut c = common::mysql::connect_root(&front.node).await;

    // a) The census on the host engine.
    common::mysql::ddl(&mut c, "CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(32), city_id INT)").await;
    common::mysql::ddl(&mut c, "CREATE TABLE cities (id INT PRIMARY KEY, name VARCHAR(32))").await;
    seed(&mut c).await;
    // b) Hot block: the join / NULL block.
    let t = scenario_common::perf::now();
    census_reads(&mut c).await;
    scenario_common::perf::within("conjnull/join-null", t, Duration::from_secs(2));
    // c) The NULL predicates (engine-agnostic, see d) plus the
    //    host-only NULL-safe `<=>`.
    null_predicates(&mut c).await;
    null_safe_equal(&mut c).await;

    // d) The SAME census in the turso library: identical expectations
    //    for every engine-agnostic read above, NULL predicates
    //    included. Only `<=>` stays host-only (1235 here).
    common::mysql::ddl(&mut c, "CREATE DATABASE turso_census").await;
    c.query_drop("USE turso_census").await.expect("use turso_census");
    common::mysql::ddl(&mut c, "CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(32), city_id INT)").await;
    common::mysql::ddl(&mut c, "CREATE TABLE cities (id INT PRIMARY KEY, name VARCHAR(32))").await;
    seed(&mut c).await;
    let t = scenario_common::perf::now();
    census_reads(&mut c).await;
    scenario_common::perf::within("conjnull/turso-join-null", t, Duration::from_secs(2));
    null_predicates(&mut c).await;
    let e = common::mysql::server_error(&mut c, "SELECT COUNT(*) FROM customers WHERE city_id <=> NULL").await;
    assert_eq!(e.code, common::mysql::ER_NOT_SUPPORTED_YET, "turso `<=>`: {e:?}");
    c.query_drop("USE rdb").await.expect("back to the host");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM customers").await, vec![6], "host census intact after the detour");

    // The library root sits outside the self-cleaning world: remove it
    // once the node is down.
    drop(front);
    drop(c);
    let _ = std::fs::remove_dir_all(&turso_dir);
    scenario_common::perf::scenario("conjnull", t0);
}
