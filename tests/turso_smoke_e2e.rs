//! T1 turso-engine smoke over one real rdb process (see
//! plans/2026-11-09-turso-engine): the routing skeleton answers
//! `SELECT <constant arithmetic>` on `turso_*` databases from the
//! embedded engine, everything engine-bound but unimplemented rejects
//! with 1235, the default (rocks) database behaves exactly as before,
//! and `USE turso_x` on a node without the engine stays 1049.

mod common;

use mysql_async::prelude::Queryable;

use common::mysql::{rows, s, spawn_node_mysql_yaml, ER_BAD_DB_ERROR, ER_NOT_SUPPORTED_YET, PASS};
use common::{
    mysql_root_conn, mysql_server_error, spawn_node_mysql, wait_mysql_ready, wait_resp_ready,
};

#[tokio::test]
async fn turso_engine_smoke_end_to_end() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-smoke-e2e-{}", std::process::id()));
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

    // Host engine first (no USE, default db): zero behavior change.
    assert_eq!(rows(&mut c, "SELECT 1+1").await, vec![vec![s("2")]]);

    // USE of a never-created turso library stays an unknown database:
    // libraries are created by CREATE DATABASE only (no auto-create
    // on USE).
    let e = mysql_server_error(&mut c, "USE turso_smoke").await;
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");

    // CREATE DATABASE materializes the library (raft catalog entry +
    // the .db file), then USE re-routes the session to it and the
    // constant projection answers from the embedded engine.
    c.query_drop("CREATE DATABASE turso_smoke")
        .await
        .expect("create database");
    c.query_drop("USE turso_smoke")
        .await
        .expect("use turso_smoke");
    assert_eq!(rows(&mut c, "SELECT 1+1").await, vec![vec![s("2")]]);

    // The library file exists on disk under the configured root.
    assert!(
        turso_dir.join("turso_smoke.db").exists(),
        "turso library file missing; node: {}",
        node.ctx()
    );

    // Constant arithmetic shapes of the T1 renderer.
    assert_eq!(rows(&mut c, "SELECT (1+2)*3").await, vec![vec![s("9")]]);
    assert_eq!(
        rows(&mut c, "SELECT -2 + 4, 'ab'").await,
        vec![vec![s("2"), s("ab")]]
    );

    // Engine-bound but still unimplemented (T3 allowlist): loud 1235,
    // never a silent fallthrough to the host engine's tables.
    let e = mysql_server_error(&mut c, "SELECT 1 UNION SELECT 2").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{e:?}");
    assert!(e.message.contains("UNION/INTERSECT/EXCEPT"), "{e:?}");
    // The T3 CRUD trunk now executes where T2 rejected: the dedicated
    // turso_crud_e2e suite carries the full matrix; here one probe per
    // shape keeps the routing skeleton honest.
    c.query_drop("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
        .await
        .expect("create table");
    c.query_drop("INSERT INTO t (id, v) VALUES (1, 'a')")
        .await
        .expect("insert");
    assert_eq!(
        rows(&mut c, "SELECT v FROM t WHERE id = 1").await,
        vec![vec![s("a")]]
    );

    // A second connection without USE keeps answering from the host
    // engine (per-session routing, not process-wide).
    let mut c2 = mysql_root_conn(&node, PASS).await;
    assert_eq!(rows(&mut c2, "SELECT 1+1").await, vec![vec![s("2")]]);

    // USE back to the host database re-routes to the host engine.
    c.query_drop("USE rdb").await.expect("use rdb");
    assert_eq!(rows(&mut c, "SELECT 1+1").await, vec![vec![s("2")]]);
}

#[tokio::test]
async fn turso_use_unknown_without_engine() {
    let dir = std::env::temp_dir().join(format!("rdb-turso-smoke-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // Stock node (no turso keys): turso_* stays an unknown database.
    let e = mysql_server_error(&mut c, "USE turso_x").await;
    assert_eq!(e.code, ER_BAD_DB_ERROR, "{e:?}");
    assert!(e.message.contains("Unknown database 'turso_x'"), "{e:?}");
    assert_eq!(rows(&mut c, "SELECT 1+1").await, vec![vec![s("2")]]);
}
