//! Multi-store scenario #139 -- warehouse schema migration: the stock
//! ledger is stock-taken with RIGHT JOIN (unmatched warehouses
//! null-extended, equal to the swapped LEFT JOIN) and CROSS JOIN (every
//! warehouse x sku pair, exact and ordered; the comma spelling is
//! refused), then `RENAME TABLE stock TO inventory` carries data and the
//! PRIMARY index under the new name (old name 1146), a secondary index
//! round trips through EXPLAIN (IndexScan -> SeqScan), and the migration
//! database `turso_wh_mig139` is created, entered, refused for
//! non-turso names and dropped while a RESP hash mirror of the ledger
//! stays equal to the SQL rows. One REAL node, MySQL + RESP.
//! Deviation: only `turso_*` databases exist as explicit objects, so the
//! brief's `wh_<tag>` becomes `turso_wh_mig139` and `CREATE DATABASE
//! wh_mig139` is 1235 (src/sql/turso/catalog.rs:59-66); `FROM a, b` is
//! refused as 1235 "comma cross joins", not a 1064 parse error
//! (src/sql/parse/query.rs:121-124).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{
    col, connect_root, ddl, one, rows, run, server_error, s, ER_BAD_DB_ERROR, ER_DB_DROP_EXISTS,
    ER_NO_SUCH_TABLE, ER_NOT_SUPPORTED_YET,
};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, r, rb, FrontSpec};

/// One wire cell as text (`<NULL>` for the NULL variant).
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

fn cells(row: &[MVal]) -> Vec<String> {
    row.iter().map(cell).collect()
}

fn text(rs: &[Vec<MVal>]) -> Vec<Vec<String>> {
    rs.iter().map(|r| cells(r)).collect()
}

/// Rows as a set (line order and column order inside a line preserved,
/// line order sorted) -- for results whose NULL placement is not pinned.
fn sorted(rs: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut rs = rs;
    rs.sort();
    rs
}

async fn index_pairs(c: &mut mysql_async::Conn) -> Vec<(String, String)> {
    let idx = rows(c, "SHOW INDEX FROM inventory").await;
    idx.iter().map(|r| (cell(&r[2]), cell(&r[4]))).collect()
}

/// Fixture, Rust side: warehouses and the stock ledger (wh 9 is an
/// orphan: no warehouse has that id; Kyiv has no stock yet).
const WAREHOUSES: [(i64, &str); 3] = [(1, "Oslo"), (2, "Lima"), (3, "Kyiv")];
const STOCK: [(i64, &str, i64); 5] = [
    (1, "BOLT", 40),
    (1, "NUT", 5),
    (2, "BOLT", 12),
    (2, "WIRE", 7),
    (9, "GHOST", 1),
];
/// The delivery booked into Kyiv after the rename.
const KYIV: (i64, &str, i64) = (3, "BOLT", 2);

/// `warehouses LEFT JOIN stock ON wh_id = id` rows: every stock row of
/// the warehouse (sku order) or one NULL-extended line when it has none.
fn outer_join_rows() -> Vec<Vec<String>> {
    WAREHOUSES
        .iter()
        .flat_map(|(id, city)| {
            let mut lines: Vec<Vec<String>> = STOCK
                .iter()
                .filter(|(wh, _, _)| wh == id)
                .map(|(_, sku, qty)| {
                    vec![
                        id.to_string(),
                        (*city).to_string(),
                        (*sku).to_string(),
                        qty.to_string(),
                    ]
                })
                .collect();
            if lines.is_empty() {
                lines.push(vec![
                    id.to_string(),
                    (*city).to_string(),
                    "<NULL>".to_string(),
                    "<NULL>".to_string(),
                ]);
            }
            lines.sort_by(|a, b| a[2].cmp(&b[2]));
            lines
        })
        .collect()
}

/// Every warehouse x stock pair in (warehouse id, sku) order.
fn cross_join_rows() -> Vec<Vec<String>> {
    let mut skus: Vec<&str> = STOCK.iter().map(|(_, sku, _)| *sku).collect();
    skus.sort_unstable();
    WAREHOUSES
        .iter()
        .flat_map(|(id, _)| {
            skus.iter()
                .map(move |sku| vec![id.to_string(), (*sku).to_string()])
        })
        .collect()
}

#[tokio::test]
async fn schema_migration_joins_rename_drop_round_trip() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let turso_dir = std::env::temp_dir().join(format!(
        "rdb-scenario-turso139-db-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&turso_dir);
    std::fs::create_dir_all(&turso_dir).expect("create turso db dir");
    let extra_yaml = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let spec = FrontSpec {
        mysql: true,
        extra_yaml,
        ..Default::default()
    };
    let (front, a) = front_world("mig139", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Host-engine schema in the default database: the warehouse list
    //    and the stock ledger (composite PRIMARY KEY).
    ddl(&mut c, "CREATE TABLE warehouses (id INT PRIMARY KEY, city VARCHAR(16))").await;
    ddl(&mut c, "CREATE TABLE stock (wh_id INT, sku VARCHAR(16), qty INT, \
                 PRIMARY KEY (wh_id, sku))")
        .await;
    let whs: Vec<String> = WAREHOUSES
        .iter()
        .map(|(id, city)| format!("({id}, '{city}')"))
        .collect();
    run(
        &mut c,
        &format!("INSERT INTO warehouses (id, city) VALUES {}", whs.join(", ")),
    )
    .await;
    let st: Vec<String> = STOCK
        .iter()
        .map(|(wh, sku, qty)| format!("({wh}, '{sku}', {qty})"))
        .collect();
    run(
        &mut c,
        &format!("INSERT INTO stock (wh_id, sku, qty) VALUES {}", st.join(", ")),
    )
    .await;

    // b) Hot block: the stock-take joins. RIGHT JOIN null-extends the
    //    warehouse without stock and equals the swapped LEFT JOIN; the
    //    inner join drops the orphan; CROSS JOIN is every pair.
    let t = scenario_common::perf::now();
    let right = text(&rows(
        &mut c,
        "SELECT w.id, w.city, s.sku, s.qty FROM stock s RIGHT JOIN warehouses w \
         ON s.wh_id = w.id ORDER BY w.id, s.sku",
    )
    .await);
    let left = text(&rows(
        &mut c,
        "SELECT w.id, w.city, s.sku, s.qty FROM warehouses w LEFT JOIN stock s \
         ON s.wh_id = w.id ORDER BY w.id, s.sku",
    )
    .await);
    assert_eq!(right, left, "RIGHT JOIN == swapped LEFT JOIN");
    assert_eq!(
        sorted(right.clone()),
        sorted(outer_join_rows()),
        "outer join rows (Rust-side expectation)"
    );
    assert_eq!(
        cells(&rows(
            &mut c,
            "SELECT w.id, w.city, s.sku, s.qty FROM stock s RIGHT JOIN warehouses w \
             ON s.wh_id = w.id WHERE w.id = 3"
        )
        .await
        .remove(0)),
        ["3", "Kyiv", "<NULL>", "<NULL>"],
        "Kyiv is null-extended"
    );
    let inner = cell(&one(
        &mut c,
        "SELECT COUNT(*) FROM warehouses w JOIN stock s ON s.wh_id = w.id",
    )
    .await)
    .parse::<usize>()
    .expect("inner count");
    let want_inner = STOCK
        .iter()
        .filter(|(wh, _, _)| WAREHOUSES.iter().any(|(id, _)| id == wh))
        .count();
    assert_eq!(inner, want_inner, "inner join drops the orphan ledger row");
    assert_eq!(right.len(), want_inner + 1, "outer join = inner + one padded row");
    assert_eq!(
        text(&rows(
            &mut c,
            "SELECT w.id, s.sku FROM warehouses w CROSS JOIN stock s ORDER BY w.id, s.sku"
        )
        .await),
        cross_join_rows(),
        "CROSS JOIN pairs (Rust-side expectation)"
    );
    assert_eq!(
        cell(&one(
            &mut c,
            "SELECT COUNT(*) FROM warehouses w CROSS JOIN stock s"
        )
        .await),
        (WAREHOUSES.len() * STOCK.len()).to_string(),
        "CROSS JOIN size"
    );
    let e = server_error(&mut c, "SELECT COUNT(*) FROM warehouses w, stock s").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "comma join: {e:?}");
    assert!(
        e.message.contains("comma cross joins"),
        "comma join: {e:?}"
    );
    scenario_common::perf::within("mig139/join-matrix", t, Duration::from_secs(2));

    // c) RENAME TABLE: same table id -- rows and the PRIMARY index follow
    //    the new name, the old name is gone (1146), SHOW TABLES flips.
    ddl(&mut c, "RENAME TABLE stock TO inventory").await;
    let tables = col(&mut c, "SHOW TABLES").await;
    assert!(tables.contains(&s("inventory")), "{tables:?}");
    assert!(tables.contains(&s("warehouses")), "{tables:?}");
    assert!(!tables.contains(&s("stock")), "{tables:?}");
    let e = server_error(&mut c, "SELECT sku FROM stock").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "{e:?}");
    assert_eq!(
        index_pairs(&mut c).await,
        vec![
            ("PRIMARY".to_string(), "wh_id".to_string()),
            ("PRIMARY".to_string(), "sku".to_string()),
        ],
        "PRIMARY index survived the rename"
    );
    run(
        &mut c,
        &format!("INSERT INTO inventory (wh_id, sku, qty) VALUES ({}, '{}', {})", KYIV.0, KYIV.1, KYIV.2),
    )
    .await;
    let ledger = text(&rows(
        &mut c,
        "SELECT wh_id, sku, qty FROM inventory ORDER BY wh_id, sku",
    )
    .await);
    let mut want_ledger: Vec<Vec<String>> = STOCK
        .iter()
        .map(|(wh, sku, qty)| vec![wh.to_string(), (*sku).to_string(), qty.to_string()])
        .collect();
    want_ledger.push(vec![
        KYIV.0.to_string(),
        KYIV.1.to_string(),
        KYIV.2.to_string(),
    ]);
    assert_eq!(sorted(ledger.clone()), sorted(want_ledger.clone()), "ledger after rename");

    // d) Secondary index round trip observed through EXPLAIN.
    ddl(&mut c, "CREATE INDEX ix_sku ON inventory (sku)").await;
    let index_pairs_now = index_pairs(&mut c).await;
    assert!(index_pairs_now.contains(&("ix_sku".to_string(), "sku".to_string())), "{index_pairs_now:?}");
    let bolts = want_ledger.iter().filter(|l| l[1] == "BOLT").count();
    let plan = col(&mut c, "EXPLAIN SELECT qty FROM inventory WHERE sku = 'BOLT'").await;
    assert_eq!(
        cell(&plan[0]),
        format!("IndexScan ix_sku -> {bolts} pks"),
        "index plan: {plan:?}"
    );
    assert_eq!(cell(&plan[1]), "Filter: sku = 'BOLT'", "{plan:?}");
    ddl(&mut c, "DROP INDEX ix_sku ON inventory").await;
    let plan = col(&mut c, "EXPLAIN SELECT qty FROM inventory WHERE sku = 'BOLT'").await;
    assert_eq!(cell(&plan[0]), "SeqScan inventory", "plan after drop: {plan:?}");
    assert_eq!(
        index_pairs(&mut c).await,
        vec![
            ("PRIMARY".to_string(), "wh_id".to_string()),
            ("PRIMARY".to_string(), "sku".to_string()),
        ],
        "only PRIMARY remains"
    );

    // e) The migration database: created, entered, then dropped; names
    //    outside turso_* and unknown drops are refused.
    ddl(&mut c, "CREATE DATABASE turso_wh_mig139").await;
    let dbs = col(&mut c, "SHOW DATABASES").await;
    assert!(dbs.contains(&s("rdb")) && dbs.contains(&s("turso_wh_mig139")), "{dbs:?}");
    run(&mut c, "USE turso_wh_mig139").await;
    assert_eq!(
        cell(&one(&mut c, "SELECT DATABASE()").await),
        "turso_wh_mig139"
    );
    let e = server_error(&mut c, "CREATE DATABASE wh_mig139").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    assert!(e.message.contains("turso_"), "{e:?}");
    let e = server_error(&mut c, "USE wh_missing").await;
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");
    let e = server_error(&mut c, "DROP DATABASE turso_wh_missing").await;
    assert_eq!(e.code, ER_DB_DROP_EXISTS, "{e:?}");
    run(&mut c, "USE rdb").await;
    ddl(&mut c, "DROP DATABASE turso_wh_mig139").await;
    let dbs = col(&mut c, "SHOW DATABASES").await;
    assert!(!dbs.contains(&s("turso_wh_mig139")), "{dbs:?}");
    assert!(dbs.contains(&s("rdb")), "{dbs:?}");

    // f) RESP mirror: one hash field per ledger line, fed from SQL; the
    //    hash length is the ledger count and the field values agree.
    for line in &ledger {
        let field = format!("{}:{}", line[0], line[1]).into_bytes();
        assert_eq!(
            int_of(&r(&a, &[b"HSET", b"mig139:stock", &field, line[2].as_bytes()]).await),
            1,
            "hset ledger line"
        );
    }
    let count = cell(&one(&mut c, "SELECT COUNT(*) FROM inventory").await);
    assert_eq!(count, want_ledger.len().to_string(), "ledger count");
    assert_eq!(int_of(&r(&a, &[b"HLEN", b"mig139:stock"]).await), want_ledger.len() as i64);
    assert_eq!(
        rb(&a, &[b"HGET", b"mig139:stock", format!("{}:{}", KYIV.0, KYIV.1).as_bytes()]).await,
        KYIV.2.to_string(),
        "Kyiv delivery mirrored"
    );
    assert_eq!(
        rb(&a, &[b"HGET", b"mig139:stock", b"2:WIRE"]).await,
        STOCK[3].2.to_string(),
        "Lima wire mirrored"
    );
    scenario_common::perf::scenario("mig139", t0);
}
