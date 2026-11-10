//! T2 turso-engine catalog surface over one real rdb process (see
//! plans/2026-11-09-turso-engine): CREATE/DROP DATABASE (file + raft
//! catalog entry), the SHOW DATABASES merge, the sqlite_master-backed
//! SHOW TABLES / DESCRIBE / SHOW CREATE TABLE, the minimal INSERT /
//! SELECT data round-trip, and the permission-decided read-only reopen.

use crate::common;

use mysql_async::prelude::Queryable;

use common::mysql::{
    rows, s, spawn_node_mysql_yaml, ER_BAD_DB_ERROR, ER_DB_CREATE_EXISTS, ER_DB_DROP_EXISTS,
    ER_NOT_SUPPORTED_YET, ER_OPTION_PREVENTS_STATEMENT, PASS,
};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready};

#[tokio::test]
async fn turso_catalog_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-cat-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let turso_dir = dir.join("tursodb");
    std::fs::create_dir_all(&turso_dir).unwrap();
    let extra = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let mut node = spawn_node_mysql_yaml(&dir, 0, true, None, &extra);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // (a) CREATE DATABASE: SHOW DATABASES merges the turso library in.
    c.query_drop("CREATE DATABASE turso_cat")
        .await
        .expect("create turso_cat");
    let dbs = common::mysql::col(&mut c, "SHOW DATABASES").await;
    assert!(dbs.contains(&s("rdb")), "{dbs:?}; node: {}", node.ctx());
    assert!(dbs.contains(&s("turso_cat")), "{dbs:?}");
    assert!(
        turso_dir.join("turso_cat.db").exists(),
        "library file missing"
    );

    // Duplicate create answers 1007; IF NOT EXISTS is a no-op.
    let e = mysql_server_error(&mut c, "CREATE DATABASE turso_cat").await;
    assert_eq!(e.code, ER_DB_CREATE_EXISTS, "{e:?}");
    c.query_drop("CREATE DATABASE IF NOT EXISTS turso_cat")
        .await
        .expect("if not exists");

    // Non-turso names and the implicit default are not creatable.
    let e = mysql_server_error(&mut c, "CREATE DATABASE foo").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    let e = mysql_server_error(&mut c, "CREATE DATABASE rdb").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    // USE of a never-created turso name stays unknown (1049).
    let e = mysql_server_error(&mut c, "USE turso_nope").await;
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");

    // (b) Table + data round-trip on the created library.
    c.query_drop("USE turso_cat").await.expect("use turso_cat");
    c.query_drop("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(32) NOT NULL)")
        .await
        .expect("create table");
    c.query_drop("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')")
        .await
        .expect("insert");
    assert_eq!(
        rows(&mut c, "SELECT * FROM t").await,
        vec![vec![s("1"), s("a")], vec![s("2"), s("b")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT name FROM t").await,
        vec![vec![s("a")], vec![s("b")]]
    );

    // SHOW TABLES lists the library's own tables.
    assert_eq!(
        common::mysql::col(&mut c, "SHOW TABLES").await,
        vec![s("t")]
    );

    // DESCRIBE renders table_info (Field/Type/Null/Key/Default/Extra).
    let desc = common::mysql::rows(&mut c, "DESCRIBE t").await;
    assert_eq!(desc.len(), 2, "{desc:?}");
    assert_eq!(desc[0][0], s("id"), "{desc:?}");
    assert_eq!(desc[0][1], s("integer"), "{desc:?}");
    assert_eq!(desc[0][2], s("NO"), "{desc:?}");
    assert_eq!(desc[0][3], s("PRI"), "{desc:?}");
    assert_eq!(desc[1][0], s("name"), "{desc:?}");
    assert_eq!(desc[1][1], s("text"), "{desc:?}");
    assert_eq!(desc[1][2], s("NO"), "{desc:?}");
    // SHOW COLUMNS FROM is the same statement; SHOW CREATE TABLE
    // returns the stored sqlite_master sql text.
    assert_eq!(
        common::mysql::rows(&mut c, "SHOW COLUMNS FROM t").await,
        desc
    );
    let sct = common::mysql::rows(&mut c, "SHOW CREATE TABLE t").await;
    assert_eq!(sct.len(), 1, "{sct:?}");
    assert_eq!(sct[0][0], s("t"), "{sct:?}");
    let ddl = match &sct[0][1] {
        mysql_async::Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => format!("{other:?}"),
    };
    assert!(ddl.to_ascii_uppercase().contains("CREATE TABLE"), "{ddl}");
    // turso stores the sql text verbatim (our renderer's quoting rides
    // along): a documented SHOW CREATE TABLE deviation.
    assert!(ddl.contains("TEXT NOT NULL"), "{ddl}");
    // DESCRIBE of a missing table keeps MySQL's 1146.
    let e = mysql_server_error(&mut c, "DESCRIBE nosuch").await;
    assert_eq!(e.code, 1146, "{e:?}");

    // (c) Permission-decided read-only reopen: stop the node, strip
    // every write bit from the library (and its WAL sibling), restart
    // on the same data dir -- reads serve, writes answer 1290.
    node.kill_now();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let p = turso_dir.join(format!("turso_cat.db{suffix}"));
        if let Ok(m) = std::fs::metadata(&p) {
            let mut perms = m.permissions();
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o444);
            std::fs::set_permissions(&p, perms).unwrap();
        }
    }
    node.respawn();
    wait_resp_ready(&mut node, 20).await;
    wait_mysql_ready(&node, 20).await;
    let mut ro = mysql_root_conn(&node, PASS).await;
    ro.query_drop("USE turso_cat")
        .await
        .expect("use read-only library");
    assert_eq!(
        rows(&mut ro, "SELECT id, name FROM t").await,
        vec![vec![s("1"), s("a")], vec![s("2"), s("b")]]
    );
    let e = mysql_server_error(&mut ro, "INSERT INTO t (id, name) VALUES (3, 'c')").await;
    assert_eq!(e.code, ER_OPTION_PREVENTS_STATEMENT, "{e:?}");
    assert!(e.message.contains("read-only"), "{e:?}");
    // A fresh CREATE DATABASE still works next to the ro library.
    ro.query_drop("CREATE DATABASE turso_rw2")
        .await
        .expect("create beside ro");

    // (d) DROP DATABASE: file (and WAL sibling) gone, listing gone,
    // USE afterwards unknown.
    ro.query_drop("USE rdb").await.expect("use rdb");
    let e = mysql_server_error(&mut ro, "DROP DATABASE nosuch").await;
    assert_eq!(e.code, ER_DB_DROP_EXISTS, "{e:?}");
    let e = mysql_server_error(&mut ro, "DROP DATABASE rdb").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    ro.query_drop("DROP DATABASE IF EXISTS never_was")
        .await
        .expect("drop if exists noop");
    ro.query_drop("DROP DATABASE turso_cat")
        .await
        .expect("drop turso_cat");
    assert!(
        !turso_dir.join("turso_cat.db").exists(),
        "library file survived drop"
    );
    let dbs = common::mysql::col(&mut ro, "SHOW DATABASES").await;
    assert!(!dbs.contains(&s("turso_cat")), "{dbs:?}");
    let e = mysql_server_error(&mut ro, "USE turso_cat").await;
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");
    // Recreate-and-drop cycle stays clean (cache eviction, no stale
    // inode serving old rows).
    ro.query_drop("CREATE DATABASE turso_cat")
        .await
        .expect("recreate");
    ro.query_drop("USE turso_cat").await.expect("reuse");
    assert_eq!(rows(&mut ro, "SELECT 1+1").await, vec![vec![s("2")]]);
    ro.query_drop("USE rdb").await.expect("back to rdb");
    ro.query_drop("DROP DATABASE turso_cat")
        .await
        .expect("re-drop");
    ro.query_drop("DROP DATABASE turso_rw2")
        .await
        .expect("drop rw2");
    let dbs = common::mysql::col(&mut ro, "SHOW DATABASES").await;
    assert_eq!(dbs, vec![s("rdb")], "{dbs:?}");
}
