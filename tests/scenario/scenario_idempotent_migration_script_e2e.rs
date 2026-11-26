//! Multi-store scenario #153 -- an idempotent schema migration script: a deploy retry
//! runs the WHOLE script twice and the second run must be a no-op (`IF [NOT] EXISTS`) or
//! an honest 1050/1062, never a silent duplicate. The users table is created with an
//! inline KEY, a secondary index is added, the data step is `INSERT ... SELECT ... WHERE
//! NOT EXISTS` (the retry inserts nothing), the rename carries table and indexes to the
//! new name, and a RESP run counter mirrors the SQL audit rows.
//! One REAL node, MySQL + RESP.
//! Deviation: `SHOW TABLES LIKE 'pat'` IGNORES the pattern -- the translator drops
//! sqlparser's filter, so every table is listed (src/sql/parse/translate.rs:489,
//! src/sql/exec/show.rs:185-200); `SHOW KEYS FROM t` answers 1235 because only the
//! SHOW INDEX spelling is mapped (src/sql/parse/translate.rs:498-509).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{col, connect_root, ddl, one, rows, server_error};
use mysql_async::Value as MVal;
use scenario_common::{front_world, int_of, ok, r, rb, rt, FrontSpec};

/// MySQL 1050 (ER_TABLE_EXISTS_ERROR): the honest answer of a plain
/// `CREATE TABLE` over an existing name.
const ER_TABLE_EXISTS: u16 = 1050;
/// MySQL 1062 (ER_DUP_ENTRY): duplicate data AND duplicate index names.
const ER_DUP_ENTRY: u16 = 1062;
/// MySQL 1054 (ER_BAD_FIELD_ERROR): an index over an unknown column.
const ER_BAD_FIELD: u16 = 1054;
/// MySQL 1146 (ER_NO_SUCH_TABLE).
const ER_NO_SUCH_TABLE: u16 = 1146;
/// MySQL 1235 (ER_NOT_SUPPORTED_YET).
const ER_NOT_SUPPORTED: u16 = 1235;

/// The users the migration carries over, Rust side: (id, name, email).
/// alice and carol share one address, so a UNIQUE index on `email`
/// cannot be built over this data.
const USERS: [(i64, &str, &str); 3] = [(1, "alice", "a@x"), (2, "bob", "b@x"), (3, "carol", "a@x")];

/// The migration version the script stamps into RESP on every run.
const VERSION: &str = "2026-11-26-1";

/// The schema the script installs (inline KEY becomes a real index).
const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS mig_users (\
                        id INT PRIMARY KEY, name VARCHAR(32) NOT NULL, \
                        email VARCHAR(64), KEY idx_name (name))";

/// One wire cell as text (`<NULL>` for the NULL variant).
fn cell(v: &MVal) -> String {
    match v {
        MVal::NULL => "<NULL>".to_string(),
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    }
}

/// Rows as text lines (column order preserved).
fn text(rs: &[Vec<MVal>]) -> Vec<Vec<String>> {
    rs.iter().map(|r| r.iter().map(cell).collect()).collect()
}

/// `(Key_name, Column_name)` pairs of `SHOW INDEX FROM <table>`.
async fn index_pairs(c: &mut mysql_async::Conn, table: &str) -> Vec<(String, String)> {
    rows(c, &format!("SHOW INDEX FROM {table}"))
        .await
        .iter()
        .map(|r| (cell(&r[2]), cell(&r[4])))
        .collect()
}

/// One run of the migration script. Every statement is idempotent, so a
/// deploy retry changes nothing in the schema and inserts no user; the
/// audit row is the one row that legitimately grows per run. Returns the
/// run number the RESP counter handed out.
async fn run_script(c: &mut mysql_async::Conn, a: &str) -> i64 {
    ddl(c, USERS_DDL).await;
    ddl(c, "CREATE INDEX IF NOT EXISTS idx_email ON mig_users (email)").await;
    ddl(c, "CREATE TABLE IF NOT EXISTS mig_log (run_no INT PRIMARY KEY)").await;
    // Data step: one row per user, inserted only while it is absent.
    for (id, name, email) in USERS {
        ddl(
            c,
            &format!(
                "INSERT INTO mig_users (id, name, email) SELECT {id}, '{name}', '{email}' \
                 WHERE NOT EXISTS (SELECT 1 FROM mig_users WHERE id = {id})"
            ),
        )
        .await;
    }
    let run_no = int_of(&r(a, &[b"INCR", b"mig153:runs"]).await);
    ddl(
        c,
        &format!("INSERT INTO mig_log (run_no) VALUES ({run_no})"),
    )
    .await;
    ok(
        &r(a, &[b"SET", b"mig153:version", VERSION.as_bytes()]).await,
        "stamp the migration version",
    );
    run_no
}

#[tokio::test]
async fn idempotent_migration_script_runs_twice() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        ..Default::default()
    };
    let (front, a) = front_world("mig153", &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: the script runs twice (a deploy retry). Both runs
    //    answer +OK end to end -- every CREATE carries IF [NOT] EXISTS --
    //    and the RESP counter hands out 1 then 2.
    let t = scenario_common::perf::now();
    let first = run_script(&mut c, &a).await;
    assert_eq!(
        rb(&a, &[b"GET", b"mig153:version"]).await,
        VERSION,
        "run 1 stamped the version"
    );
    let second = run_script(&mut c, &a).await;
    scenario_common::perf::within("mig153/two-runs", t, Duration::from_secs(2));
    assert_eq!((first, second), (1, 2), "one counter bump per run");

    // b) The retry duplicated nothing: one row per user, exactly the
    //    Rust-side fixture, and the audit table holds one row per run --
    //    as many as the RESP counter counted.
    assert_eq!(
        text(&rows(&mut c, "SELECT id, name, email FROM mig_users ORDER BY id").await),
        USERS
            .iter()
            .map(|(id, name, email)| {
                vec![id.to_string(), (*name).to_string(), (*email).to_string()]
            })
            .collect::<Vec<_>>(),
        "the data step inserted each user exactly once"
    );
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM mig_users").await),
        USERS.len().to_string(),
        "no duplicate users after two runs"
    );
    let runs: i64 = rb(&a, &[b"GET", b"mig153:runs"])
        .await
        .parse()
        .expect("the run counter is a decimal bulk");
    assert_eq!(runs, 2, "two runs, two counter bumps");
    assert_eq!(
        col(&mut c, "SELECT run_no FROM mig_log ORDER BY run_no").await,
        vec![MVal::Bytes(b"1".to_vec()), MVal::Bytes(b"2".to_vec())],
        "the audit table logged both runs"
    );
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM mig_log").await),
        runs.to_string(),
        "SQL row count equals the RESP counter"
    );
    assert_eq!(
        rb(&a, &[b"GET", b"mig153:version"]).await,
        VERSION,
        "the retry left the version key unchanged"
    );
    assert_eq!(
        rt(&a, &[b"TYPE", b"mig153:version"]).await,
        "+string",
        "the version key is a plain string"
    );

    // c) The non-idempotent spellings answer honestly instead of
    //    silently duplicating: a plain CREATE TABLE is 1050, a plain
    //    CREATE INDEX over an existing name is 1062, and the IF NOT
    //    EXISTS spelling stays +OK (a third, standalone run).
    ddl(&mut c, USERS_DDL).await;
    let e = server_error(
        &mut c,
        "CREATE TABLE mig_users (id INT PRIMARY KEY, name VARCHAR(32) NOT NULL, \
         email VARCHAR(64), KEY idx_name (name))",
    )
    .await;
    assert_eq!(e.code, ER_TABLE_EXISTS, "plain create: {e:?}");
    assert!(e.message.contains("already exists"), "{e:?}");
    ddl(&mut c, "CREATE INDEX IF NOT EXISTS idx_email ON mig_users (email)").await;
    let e = server_error(&mut c, "CREATE INDEX idx_email ON mig_users (email)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "plain create index: {e:?}");
    assert!(
        e.message.contains("index 'idx_email' already exists"),
        "{e:?}"
    );

    // d) The inline KEY became a real index: SHOW INDEX lists it next to
    //    the PRIMARY and the added secondary index, and the planner reads
    //    through it (one pk behind 'alice').
    let pairs = index_pairs(&mut c, "mig_users").await;
    for want in [
        ("PRIMARY".to_string(), "id".to_string()),
        ("idx_name".to_string(), "name".to_string()),
        ("idx_email".to_string(), "email".to_string()),
    ] {
        assert!(pairs.contains(&want), "index {want:?} missing: {pairs:?}");
    }
    let plan = col(&mut c, "EXPLAIN SELECT id FROM mig_users WHERE name = 'alice'").await;
    assert_eq!(
        plan.iter().map(cell).collect::<Vec<_>>(),
        vec![
            "IndexScan idx_name -> 1 pks".to_string(),
            "Filter: name = 'alice'".to_string(),
            "Project: id".to_string(),
        ],
        "the inline KEY is the access path"
    );
    let e = server_error(&mut c, "SHOW KEYS FROM mig_users").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED, "SHOW KEYS: {e:?}");
    assert!(e.message.contains("SHOW variables"), "{e:?}");

    // e) The ALTER family refuses the impossible migrations loudly.
    let e = server_error(&mut c, "ALTER TABLE mig_users ADD UNIQUE INDEX uq_email (email)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "unique over duplicate data: {e:?}");
    assert!(e.message.contains("a@x"), "{e:?}");
    let e = server_error(&mut c, "ALTER TABLE mig_users ADD INDEX bad (nope)").await;
    assert_eq!(e.code, ER_BAD_FIELD, "index over an unknown column: {e:?}");
    let e = server_error(&mut c, "ALTER TABLE mig_nope ADD INDEX x (id)").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "alter on an unknown table: {e:?}");
    let e = server_error(&mut c, "ALTER TABLE mig_users ADD INDEX idx_name (name)").await;
    assert_eq!(e.code, ER_DUP_ENTRY, "duplicate index via ALTER: {e:?}");

    // f) RENAME carries the table and its indexes: both accepted
    //    spellings answer, the old name is gone (1146) and the new one
    //    serves the same rows.
    ddl(&mut c, "RENAME TABLE mig_users TO mig_accounts").await;
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM mig_accounts").await),
        USERS.len().to_string(),
        "rows follow the rename"
    );
    assert!(
        index_pairs(&mut c, "mig_accounts")
            .await
            .contains(&("idx_name".to_string(), "name".to_string())),
        "the inline KEY followed the rename"
    );
    let e = server_error(&mut c, "SELECT id FROM mig_users").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "the old name is gone: {e:?}");
    ddl(&mut c, "ALTER TABLE mig_accounts RENAME AS mig_users").await;
    assert_eq!(
        cell(&one(&mut c, "SELECT COUNT(*) FROM mig_users").await),
        USERS.len().to_string(),
        "the table answers under its old name again"
    );

    // g) `SHOW TABLES LIKE 'mig%'` (deviation): the pattern is ignored,
    //    so the non-matching audit table is listed as well -- the answer
    //    equals the unfiltered SHOW TABLES.
    ddl(&mut c, "CREATE TABLE IF NOT EXISTS zz_audit (id INT PRIMARY KEY)").await;
    let mut filtered = col(&mut c, "SHOW TABLES LIKE 'mig%'").await;
    let mut every = col(&mut c, "SHOW TABLES").await;
    filtered.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    every.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    assert_eq!(filtered, every, "LIKE is not applied (deviation)");
    assert!(
        filtered.contains(&MVal::Bytes(b"zz_audit".to_vec())),
        "the non-matching table leaked into the LIKE answer: {filtered:?}"
    );
    assert!(
        filtered.contains(&MVal::Bytes(b"mig_users".to_vec())),
        "{filtered:?}"
    );
    scenario_common::perf::scenario("mig153", t0);
}
