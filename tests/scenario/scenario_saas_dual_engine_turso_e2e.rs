//! Batch 11 scenario #120 -- a SaaS tenant CRM split across BOTH
//! engines of one REAL node: the host (rocks) database keeps the
//! default catalog while `turso_crm` materializes as a turso library
//! (file + raft catalog entry), `SHOW DATABASES` merges the two, and
//! the turso session runs a real workload -- AUTO_INCREMENT insert
//! with `LAST_INSERT_ID()`, a JOIN, a UNION and a CTE -- next to the
//! T3 allowlist's six still-rejected shapes (loud 1235, never a silent
//! fallthrough to the host engine's tables). `USE turso_nope` stays
//! 1049. One REAL node, MySQL front + the embedded turso engine.

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{
    col, connect_root, ddl, rows, server_error, s, ER_BAD_DB_ERROR, ER_DB_CREATE_EXISTS,
    ER_NOT_SUPPORTED_YET,
};
use mysql_async::prelude::Queryable;
use scenario_common::{front_world, FrontSpec};

#[tokio::test]
async fn saas_crm_runs_on_both_engines_of_one_node() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    // The turso library root must live OUTSIDE the scenario world dir
    // (`front_world` wipes that first), so create it before spawning.
    let turso_dir = std::env::temp_dir().join(format!(
        "rdb-scenario-turso120-db-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&turso_dir);
    std::fs::create_dir_all(&turso_dir).expect("create turso db dir");
    let extra_yaml = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    assert!(turso_dir.is_absolute(), "turso_db_dir must be absolute");
    let spec = FrontSpec {
        mysql: true,
        extra_yaml,
        ..Default::default()
    };
    let (front, _a) = front_world("turso120", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: exactly 20 statements -- the catalog entry, the two
    //    CRM tables, six inserts, LAST_INSERT_ID, the JOIN / UNION /
    //    CTE projections, and the six 1235 rejections (each probe is
    //    one statement), then the unknown-library USE.
    let t = scenario_common::perf::now();
    ddl(&mut c, "CREATE DATABASE turso_crm").await; // 1
    c.query_drop("USE turso_crm").await.expect("use turso_crm");
    c.query_drop(
        "CREATE TABLE crm_deals (id BIGINT PRIMARY KEY, account_id BIGINT NOT NULL, amount INT)",
    )
    .await
    .expect("create deals"); // 2
    c.query_drop("CREATE TABLE crm_accounts (id BIGINT AUTO_INCREMENT PRIMARY KEY, \
                  name VARCHAR(32) NOT NULL)")
        .await
        .expect("create accounts"); // 3
    for (id, account, amount) in [(1, 1, 100), (2, 2, 200), (3, 3, 50)] {
        c.query_drop(format!(
            "INSERT INTO crm_deals (id, account_id, amount) VALUES ({id}, {account}, {amount})"
        ))
        .await
        .expect("insert deal"); // 4, 5, 6
    }
    for name in ["ada", "bob", "cyd"] {
        c.query_drop(format!("INSERT INTO crm_accounts (name) VALUES ('{name}')"))
            .await
            .expect("insert account"); // 7, 8, 9
    }
    let last = col(&mut c, "SELECT LAST_INSERT_ID()").await; // 10
    let join = rows(
        &mut c,
        "SELECT a.name, d.amount FROM crm_accounts a JOIN crm_deals d ON d.account_id = a.id \
         ORDER BY a.id",
    )
    .await; // 11
    let union = col(
        &mut c,
        "SELECT id FROM crm_deals UNION SELECT id FROM crm_accounts ORDER BY id",
    )
    .await; // 12
    let cte = col(
        &mut c,
        "WITH big AS (SELECT id FROM crm_deals WHERE amount >= 100) SELECT id FROM big ORDER BY id",
    )
    .await; // 13
    let mut rejected = Vec::new();
    for sql in [
        "SELECT id FROM crm_accounts WHERE id <=> 1",                        // 14
        "SELECT LEFT('abc', 2)",                                             // 15
        "SELECT name, GROUP_CONCAT(name) FROM crm_accounts GROUP BY name",   // 16
        "CREATE TABLE vc (id INT PRIMARY KEY, v VECTOR(3) NOT NULL) ENGINE=columnar", // 17
        "SELECT 1 INTERSECT ALL SELECT 2",                                   // 18
        "SELECT t.n FROM (SELECT name AS n FROM crm_accounts) AS t",         // 19
    ] {
        let e = server_error(&mut c, sql).await;
        assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{sql}: {e:?}");
        rejected.push((sql, e.message));
    }
    let e = server_error(&mut c, "USE turso_nope").await; // 20
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");
    scenario_common::perf::within("turso120/20-statements", t, Duration::from_secs(3));

    // b) The workload answers: 1..3 auto ids, so LAST_INSERT_ID() is 3
    //    (the LAST of the statement on turso), the JOIN pairs every
    //    account with its deal, the UNION dedupes 1..3, and the CTE
    //    filters the two big deals.
    assert_eq!(last, vec![s("3")], "LAST_INSERT_ID after three inserts");
    assert_eq!(
        join,
        vec![
            vec![s("ada"), s("100")],
            vec![s("bob"), s("200")],
            vec![s("cyd"), s("50")],
        ],
        "{join:?}"
    );
    assert_eq!(union, vec![s("1"), s("2"), s("3")], "{union:?}");
    assert_eq!(cte, vec![s("1"), s("2")], "{cte:?}");
    for (sql, msg) in &rejected {
        assert!(
            msg.contains("not supported in this version"),
            "{sql}: {msg}"
        );
    }

    // c) The library file landed in the configured root (absolute path
    //    honored), and the catalog merges both databases.
    assert!(
        turso_dir.join("turso_crm.db").exists(),
        "library file missing in {turso_dir:?}"
    );
    let dbs = col(&mut c, "SHOW DATABASES").await;
    assert!(dbs.contains(&s("rdb")), "{dbs:?}");
    assert!(dbs.contains(&s("turso_crm")), "{dbs:?}");

    // d) Duplicate create is 1007, non-turso names are not creatable
    //    (1235), and the default database is still the host engine's.
    let e = server_error(&mut c, "CREATE DATABASE turso_crm").await;
    assert_eq!(e.code, ER_DB_CREATE_EXISTS, "{e:?}");
    let e = server_error(&mut c, "CREATE DATABASE foo").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    let e = server_error(&mut c, "CREATE DATABASE rdb").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");

    // e) USE back to the host database re-routes the SAME session: the
    //    CRM tables are not visible there (they live in the library),
    //    while the constant projection answers from the host engine.
    c.query_drop("USE rdb").await.expect("use rdb");
    assert_eq!(rows(&mut c, "SELECT 1+1").await, vec![vec![s("2")]], "host engine");
    let e = server_error(&mut c, "SELECT id FROM crm_accounts").await;
    assert_eq!(e.code, 1146, "host engine cannot see the library: {e:?}");

    // f) A second connection without USE stays on the host engine
    //    (per-session routing, not process-wide).
    let mut c2 = connect_root(&front.node).await;
    assert_eq!(rows(&mut c2, "SELECT 1+1").await, vec![vec![s("2")]]);
    c2.query_drop("USE turso_crm").await.expect("c2 use turso_crm");
    assert_eq!(
        col(&mut c2, "SELECT name FROM crm_accounts ORDER BY id").await,
        vec![s("ada"), s("bob"), s("cyd")],
        "library data is shared across sessions"
    );

    scenario_common::perf::scenario("turso120", t0);
}
