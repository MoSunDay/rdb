//! Multi-store scenario #155 -- a SaaS tenant lifecycle on the embedded turso engine
//! of one node: CREATE DATABASE (library file + catalog entry) -> USE -> the
//! composite-PK tenants table with an inline KEY -> inserts -> ALTER TABLE ADD INDEX /
//! DROP INDEX -> AUTO_INCREMENT 1,2 and TRUNCATE keeping the counter -> the error map
//! (1062 duplicate composite key, 1048 NOT NULL, 1235 bad column options / ALTER ADD
//! COLUMN, 1146 unknown table, 1290 read-only library, 1049 / 1007 / 1008 catalog
//! codes) -> DDL inside a transaction ALLOWED on turso (1235 on the host engine) ->
//! DROP DATABASE (file gone) while RESP mirrors the tenant name.
//! MySQL (turso engine + host) + RESP. One REAL node, fronts mysql + the embedded turso
//! engine (turso_engine + turso_db_dir outside the world dir).
//! Deviation: creatable libraries must be named `turso_*` (src/sql/turso/catalog.rs:58-66), so the
//! library is `turso_tenant155`; DEFAULT clauses are dropped (src/sql/parse/translate_type.rs:35).

use std::time::Duration;

use crate::common;
use crate::scenario_common;
use common::mysql::{col, connect_root, ddl, int, rows, server_error, s};
use mysql_async::prelude::Queryable;
use mysql_async::Value as MVal;
use scenario_common::{FrontSpec, front_world, r, rb};

const TAG: &str = "turso155";
const DB: &str = "turso_tenant155";
const RESP_KEY: &str = "tenant:155:db";
/// The sqlite_master text SHOW CREATE TABLE hands back: every identifier is
/// double-quoted by the renderer (src/sql/turso/translate/ddl.rs:61-80) and
/// the inline KEY is NOT part of it (it becomes a follow-up CREATE INDEX).
const CREATE_TEXT: &str = "CREATE TABLE \"tenants\" (\"tenant_id\" INTEGER NOT NULL, \
                          \"id\" INTEGER NOT NULL, \"email\" TEXT NOT NULL, \
                          \"plan\" TEXT NOT NULL, PRIMARY KEY (\"tenant_id\", \"id\"))";
// MySQL errnos pinned by this scenario (local consts).
const ER_DUP_ENTRY: u16 = 1062;
const ER_BAD_NULL: u16 = 1048;
const ER_NOT_SUPPORTED: u16 = 1235;
const ER_NO_SUCH_TABLE: u16 = 1146;
const ER_BAD_DB: u16 = 1049;
const ER_DB_CREATE_EXISTS: u16 = 1007;
const ER_DB_DROP_EXISTS: u16 = 1008;
const ER_OPTION_PREVENTS: u16 = 1290;

/// The server error of `sql` with its errno asserted; returns the message.
async fn expect_error(c: &mut mysql_async::Conn, sql: &str, code: u16) -> String {
    let e = server_error(c, sql).await;
    assert_eq!(e.code, code, "{sql}: {e:?}");
    e.message
}

/// Text of one wire cell.
fn text(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        MVal::NULL => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn turso_tenant_lifecycle_and_errors() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    // The library root lives OUTSIDE the scenario world dir (front_world
    // wipes that first), so it is created before the spawn.
    let turso_dir = std::env::temp_dir().join(format!("rdb-scenario-{TAG}-db-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&turso_dir);
    std::fs::create_dir_all(&turso_dir).expect("create turso db dir");
    let spec = FrontSpec {
        mysql: true,
        extra_yaml: format!("turso_engine: true\nturso_db_dir: \"{}\"\n", turso_dir.display()),
        ..Default::default()
    };
    let (front, a) = front_world(TAG, &spec).await;
    let mut c = connect_root(&front.node).await;

    // a) Hot block: the tenant lifecycle -- the catalog entry, USE, the
    //    composite-PK table, inserts, AUTO_INCREMENT + TRUNCATE, the
    //    schema evolution, the error ladder, the DDL-in-transaction
    //    contrast, the RESP mirror and the drop.
    let t = scenario_common::perf::now();
    let msg = expect_error(&mut c, "CREATE DATABASE tenant155", ER_NOT_SUPPORTED).await;
    assert!(msg.contains("only turso_"), "{msg}");
    ddl(&mut c, &format!("CREATE DATABASE {DB}")).await;
    let file_there = turso_dir.join(format!("{DB}.db")).is_file();
    let re_create = expect_error(&mut c, &format!("CREATE DATABASE {DB}"), ER_DB_CREATE_EXISTS).await;
    c.query_drop(format!("USE {DB}")).await.expect("use tenant library");
    ddl(&mut c, "CREATE TABLE tenants (tenant_id INT NOT NULL, id INT NOT NULL, \
                  email VARCHAR(64) NOT NULL, plan VARCHAR(16) NOT NULL DEFAULT 'free', \
                  PRIMARY KEY (tenant_id, id), KEY idx_email (email))")
        .await;
    let desc = rows(&mut c, "DESCRIBE tenants").await;
    let sct = rows(&mut c, "SHOW CREATE TABLE tenants").await;
    for (tid, id, mail) in [(1, 1, "ada@acme.test"), (1, 2, "bob@acme.test")] {
        c.query_drop(format!("INSERT INTO tenants VALUES ({tid}, {id}, '{mail}', 'pro')"))
            .await
            .expect("insert tenant user");
    }
    ddl(&mut c, "CREATE TABLE invoices (id INT AUTO_INCREMENT PRIMARY KEY, \
                  tenant_id INT NOT NULL, amount INT)")
        .await;
    for amount in [100, 250] {
        c.query_drop(format!("INSERT INTO invoices (tenant_id, amount) VALUES (1, {amount})"))
            .await
            .expect("insert invoice");
    }
    let ids = col(&mut c, "SELECT id FROM invoices ORDER BY id").await;
    c.query_drop("TRUNCATE TABLE invoices").await.expect("truncate");
    c.query_drop("INSERT INTO invoices (tenant_id, amount) VALUES (1, 300)")
        .await
        .expect("insert after truncate");
    let after_truncate = col(&mut c, "SELECT id FROM invoices").await;
    // Schema evolution on the live library.
    ddl(&mut c, "ALTER TABLE invoices ADD INDEX idx_amount (amount)").await;
    let msg = expect_error(
        &mut c,
        "ALTER TABLE invoices ADD COLUMN note VARCHAR(32)",
        ER_NOT_SUPPORTED,
    )
    .await;
    assert!(msg.contains("ALTER TABLE"), "{msg}");
    ddl(&mut c, "DROP INDEX idx_amount ON invoices").await;
    // The error ladder.
    let dup = expect_error(
        &mut c,
        "INSERT INTO tenants VALUES (1, 1, 'dup@acme.test', 'free')",
        ER_DUP_ENTRY,
    )
    .await;
    let null = expect_error(
        &mut c,
        "INSERT INTO tenants (tenant_id, id, email) VALUES (1, 3, NULL)",
        ER_BAD_NULL,
    )
    .await;
    let unsigned = expect_error(
        &mut c,
        "CREATE TABLE bad_opts (id INT PRIMARY KEY, v INT UNSIGNED)",
        ER_NOT_SUPPORTED,
    )
    .await;
    let bad_ai = expect_error(
        &mut c,
        "CREATE TABLE bad_ai (tenant_id INT, id INT AUTO_INCREMENT, PRIMARY KEY (tenant_id, id))",
        ER_NOT_SUPPORTED,
    )
    .await;
    let no_table = expect_error(&mut c, "SELECT * FROM nope", ER_NO_SUCH_TABLE).await;
    let no_desc = expect_error(&mut c, "DESCRIBE nope", ER_NO_SUCH_TABLE).await;
    // DDL inside a transaction: ALLOWED on the turso engine (the
    // transaction lives on the pinned engine connection).
    for sql in [
        "BEGIN",
        "CREATE TABLE audit_log (id INT PRIMARY KEY, note VARCHAR(64))",
        "INSERT INTO audit_log VALUES (1, 'created-in-txn')",
        "COMMIT",
    ] {
        c.query_drop(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    for sql in ["BEGIN", "INSERT INTO audit_log VALUES (2, 'rolled-back')", "ROLLBACK"] {
        c.query_drop(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let audit = col(&mut c, "SELECT note FROM audit_log ORDER BY id").await;
    // Contrast: the same DDL inside a transaction on the HOST engine.
    c.query_drop("USE rdb").await.expect("use host db");
    c.query_drop("BEGIN").await.expect("host begin");
    let host_ddl = expect_error(
        &mut c,
        "CREATE TABLE host_in_txn (id INT PRIMARY KEY)",
        ER_NOT_SUPPORTED,
    )
    .await;
    c.query_drop("ROLLBACK").await.expect("host rollback");
    c.query_drop(format!("USE {DB}")).await.expect("back to the library");
    // The RESP mirror names the tenant exactly as the catalog does.
    scenario_common::ok(&r(&a, &[b"SET", RESP_KEY.as_bytes(), DB.as_bytes()]).await, "set mirror");
    let mirrored = rb(&a, &[b"GET", RESP_KEY.as_bytes()]).await;
    let dbs_before = col(&mut c, "SHOW DATABASES").await;
    // Drop the tenant library from the host database.
    c.query_drop("USE rdb").await.expect("use host db");
    ddl(&mut c, &format!("DROP DATABASE {DB}")).await;
    let file_gone = !turso_dir.join(format!("{DB}.db")).exists();
    let use_after = expect_error(&mut c, &format!("USE {DB}"), ER_BAD_DB).await;
    let re_drop = expect_error(&mut c, &format!("DROP DATABASE {DB}"), ER_DB_DROP_EXISTS).await;
    scenario_common::perf::within("turso155/tenant-lifecycle", t, Duration::from_secs(2));

    // b) The shapes: DESCRIBE (SQLite declared types lowercased, PRI on
    //    both pk columns, no secondary key reporting, Default NULL since
    //    DEFAULT clauses never reach the engine), SHOW CREATE TABLE = the
    //    stored sqlite_master text.
    assert_eq!(
        desc,
        vec![
            vec![s("tenant_id"), s("integer"), s("NO"), s("PRI"), s("NULL"), s("")],
            vec![s("id"), s("integer"), s("NO"), s("PRI"), s("NULL"), s("")],
            vec![s("email"), s("text"), s("NO"), s(""), s("NULL"), s("")],
            vec![s("plan"), s("text"), s("NO"), s(""), s("NULL"), s("")],
        ],
        "{desc:?}"
    );
    assert_eq!(sct.len(), 1, "{sct:?}");
    assert_eq!(text(&sct[0][0]), "tenants", "{sct:?}");
    assert_eq!(text(&sct[0][1]), CREATE_TEXT, "{sct:?}");

    // c) AUTO_INCREMENT: ids 1,2, then TRUNCATE (an engine DELETE) keeps
    //    the watermark -- the next id is 3, where MySQL restarts at 1.
    assert_eq!(ids, vec![int(1), int(2)], "{ids:?}");
    assert_eq!(after_truncate, vec![int(3)], "{after_truncate:?}");

    // d) The error ladder carries the engine's own wording under MySQL errnos.
    // turso words a composite-PK clash as `tenants.(tenant_id, id)` and the
    // mapper echoes it verbatim after the errno swap (engine.rs:229-237).
    assert_eq!(dup, "Duplicate entry for key 'tenants.(tenant_id, id)'");
    assert!(null.contains("Column 'tenants.email' cannot be null"), "{null}");
    // INT UNSIGNED is the parser's own loud rejection (both engines:
    // translate_type.rs:86-90 under the parse/error.rs:80 prefix).
    assert_eq!(unsigned, "not supported in this version: column type INT UNSIGNED (v1: BOOL/INT/DOUBLE/DECIMAL/VARCHAR/BLOB/DATE/DATETIME/TIMESTAMP/VECTOR)");
    assert!(bad_ai.contains("must be the single INTEGER PRIMARY KEY"), "{bad_ai}");
    assert!(no_table.contains("no such table: nope"), "{no_table}");
    assert_eq!(no_desc, "table 'nope' doesn't exist");
    assert_eq!(host_ddl, "DDL not allowed inside a transaction");
    assert_eq!(audit, vec![s("created-in-txn")], "commit kept, rollback dropped: {audit:?}");
    assert!(file_there, "library file right after CREATE DATABASE");
    assert!(re_create.contains("database exists"), "{re_create}");
    assert_eq!(use_after, format!("Unknown database '{DB}'"));
    assert!(
        re_drop.contains("doesn't exist"),
        "{re_drop}"
    );

    // e) Cross-store: the RESP mirror value IS the catalog name before
    //    the drop, and both faces agree the tenant is gone after it.
    assert_eq!(mirrored, DB);
    let names: Vec<String> = dbs_before.iter().map(text).collect();
    assert!(names.iter().any(|n| n == DB), "{names:?}");
    assert!(names.iter().any(|n| n == "rdb"), "{names:?}");
    scenario_common::ok(
        &r(&a, &[b"SET", b"tenant:155:state", b"dropped"]).await,
        "set state",
    );
    assert_eq!(rb(&a, &[b"GET", b"tenant:155:state"]).await, "dropped");
    let dbs_after: Vec<String> = col(&mut c, "SHOW DATABASES").await.iter().map(text).collect();
    assert!(!dbs_after.iter().any(|n| n == DB), "{dbs_after:?}");
    assert!(file_gone, "library file must be unlinked by DROP DATABASE");

    // f) 1290 is the read-only library path: a 0444 library file in the
    //    root is listable/usable, but every write answers the errno.
    let ro = turso_dir.join("turso_ro155.db");
    std::fs::write(&ro, []).expect("seed read-only library");
    let mut perms = std::fs::metadata(&ro).expect("ro metadata").permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
    }
    std::fs::set_permissions(&ro, perms).expect("strip write bits");
    c.query_drop("USE turso_ro155").await.expect("use read-only library");
    let ro_err = expect_error(
        &mut c,
        "CREATE TABLE t (id INT PRIMARY KEY)",
        ER_OPTION_PREVENTS,
    )
    .await;
    c.query_drop("USE rdb").await.expect("leave read-only library");
    let _ = std::fs::remove_file(&ro);
    assert!(ro_err.contains("read-only: turso_ro155.db"), "{ro_err}");
    let e = server_error(&mut c, "DROP DATABASE rdb").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED, "{e:?}");
    scenario_common::perf::scenario(TAG, t0);
}
