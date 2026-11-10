//! WP2 turso-engine interop e2e: real sqlite3-CLI access to the very
//! library files rdb's embedded turso engine writes, in BOTH directions
//! (interop plan D1: interop = the SQLite FILE FORMAT, reachable by any
//! stock SQLite binding -- the host `sqlite3` CLI here; turso's private
//! hrana/sqld wire protocol is explicitly NOT a goal). Direction A:
//! rows MySQL writes into a `turso_*` library live in the WAL; while
//! rdb runs, an external stock reader is REFUSED (SQLITE_BUSY -- turso
//! keeps the WAL with no `-shm` file, so the library is
//! writer-exclusive, rdb the sole writer by construction), and after a
//! graceful SIGTERM the same CLI recovers the WAL on open and reads the
//! exact rows (D3: the observed access posture, documented -- no
//! checkpoint rework). Direction B: a library the CLI pre-created in
//! `turso_db_dir` is adopted via CREATE DATABASE IF NOT EXISTS (the
//! file-exists gate) and read back over MySQL, vector blobs crossing
//! as raw little-endian f32. Hosts without the `sqlite3` binary skip
//! both tests with a note (the suite degrades gracefully).

use crate::common;

use std::path::Path;
use std::time::{Duration, Instant};

use mysql_async::prelude::Queryable;

use common::mysql::{ddl, rows, run, s, spawn_node_mysql_yaml, ER_DB_CREATE_EXISTS, PASS};
use common::{mysql_root_conn, mysql_server_error, wait_mysql_ready, wait_resp_ready, ProcNode};

/// Host sqlite3 CLI present? Skipping (not failing) on hosts without
/// it keeps the suite a soft e2e dependency.
fn sqlite3_cli() -> bool {
    std::process::Command::new("sqlite3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the host sqlite3 CLI against `db` with the given INPUT LINES
/// (the CLI treats each argv element as one line, so a dot-command
/// must be its own arg), returning (exit code, trimmed stdout,
/// trimmed stderr) without asserting -- the alive-phase probes below
/// EXPECT failure.
fn sqlite3_try(db: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let out = std::process::Command::new("sqlite3")
        .arg(db)
        .args(args)
        .output()
        .expect("spawn sqlite3");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

/// The success-path read: one SQL line, trimmed stdout, panics on a
/// non-zero exit.
fn sqlite3(db: &Path, sql: &str) -> String {
    let (code, out, err) = sqlite3_try(db, &[sql]);
    assert!(code == Some(0), "sqlite3 {db:?} {sql:?}: {err}");
    out
}

/// Send a REAL SIGTERM via the shell `kill` (the harness kill_now is
/// SIGKILL, useless for graceful shutdown), then wait bounded (10s)
/// for the child to exit on its own; panics with the stderr tail if
/// the signal is ignored (process_sigterm_e2e pattern).
async fn sigterm_and_wait_exit(node: &mut ProcNode) -> i32 {
    let pid = node.child.id().to_string();
    let kill = std::process::Command::new("kill")
        .args(["-s", "TERM", pid.as_str()])
        .status()
        .expect("spawn kill");
    assert!(kill.success(), "kill -s TERM {pid} failed: {kill}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(st) = node.child.try_wait().expect("try wait child") {
            return st.code().unwrap_or(-1);
        }
        assert!(
            Instant::now() < deadline,
            "rdb still alive 10s after SIGTERM\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The two CLI reads Direction A makes against the quiescent (rdb
/// exited) library: the plain rows, and the raw LE f32 layout of a
/// stored vector (cross-engine vector evidence: 0000803F0000004000004040
/// is exactly [1.0, 2.0, 3.0] as three LE f32s).
fn assert_cli_reads(db: &Path, phase: &str) {
    assert_eq!(
        sqlite3(db, "SELECT id || '=' || v FROM t ORDER BY id"),
        "1=a\n2=b",
        "{phase}: t rows not co-readable"
    );
    assert_eq!(
        sqlite3(db, "SELECT hex(v) FROM vecs WHERE id = 1"),
        "0000803F0000004000004040",
        "{phase}: vector blob layout drifted"
    );
}

#[tokio::test]
async fn mysql_writes_sqlite3_reads() {
    if !sqlite3_cli() {
        eprintln!("skipping turso_sqlite_interop_e2e: no host sqlite3 CLI");
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("rdb-turso-sqlite-interop-a-{}", std::process::id()));
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

    // Seed over MySQL: two plain rows plus one vector row.
    ddl(&mut c, "CREATE DATABASE turso_ioa").await;
    c.query_drop("USE turso_ioa").await.expect("use turso_ioa");
    ddl(&mut c, "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").await;
    run(&mut c, "INSERT INTO t (id, v) VALUES (1, 'a'), (2, 'b')").await;
    ddl(
        &mut c,
        "CREATE TABLE vecs (id INTEGER PRIMARY KEY, v VECTOR(3) NOT NULL)",
    )
    .await;
    run(&mut c, "INSERT INTO vecs VALUES (1, '[1,2,3]')").await;

    // Alive co-read: refused. Turso keeps the WAL with NO `-shm` file
    // and holds the database locks for the process lifetime, so a
    // stock external reader cannot even PREPARE (SQLITE_BUSY, no rows
    // emitted). The 3s busy-timeout probe fails identically: a held
    // lock, not transient contention. This IS the access posture (D3):
    // external readers attach only once rdb exits; rdb is the sole
    // writer.
    let db = turso_dir.join("turso_ioa.db");
    assert!(db.is_file(), "library file missing: {db:?}");
    let (code, out, err) = sqlite3_try(&db, &["SELECT id || '=' || v FROM t ORDER BY id"]);
    assert_ne!(code, Some(0), "alive co-read succeeded: {out}");
    assert_eq!(out, "", "a refused read must not emit rows");
    assert!(
        err.contains("database is locked"),
        "alive co-read refusal: {err}"
    );
    let (code, out, err) = sqlite3_try(&db, &[".timeout 3000", "SELECT count(*) FROM t"]);
    assert_ne!(code, Some(0), "busy-timeout probe succeeded: {out}");
    assert!(
        err.contains("database is locked"),
        "3s busy-timeout must not unlock: {err}"
    );

    // Graceful shutdown, then the same reads against the quiescent
    // file: whatever still lived in the -wal must be recovered on open.
    let code = sigterm_and_wait_exit(&mut node).await;
    assert_eq!(code, 0, "SIGTERM must exit 0\n{}", node.ctx());
    assert_cli_reads(&db, "after SIGTERM");
}

#[tokio::test]
async fn sqlite3_prebuilds_mysql_reads() {
    if !sqlite3_cli() {
        eprintln!("skipping turso_sqlite_interop_e2e: no host sqlite3 CLI");
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("rdb-turso-sqlite-interop-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let turso_dir = dir.join("tursodb");
    std::fs::create_dir_all(&turso_dir).unwrap();

    // The external writer of record builds the library BEFORE rdb even
    // exists: one BLOB row holding [1.0, 2.0] as raw LE f32 plus a
    // plain TEXT sanity row.
    sqlite3(
        &turso_dir.join("turso_ext.db"),
        "CREATE TABLE ext (id INTEGER PRIMARY KEY, v BLOB); \
         INSERT INTO ext VALUES (1, X'0000803F00000040'); \
         INSERT INTO ext VALUES (2, 'plain');",
    );

    let extra = format!(
        "turso_engine: true\nturso_db_dir: \"{}\"\n",
        turso_dir.display()
    );
    let mut node = spawn_node_mysql_yaml(&dir, 0, true, None, &extra);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;

    // Adoption: the file-exists gate (path.is_file()) accepts the
    // pre-made library; the plain spelling stays 1007.
    ddl(&mut c, "CREATE DATABASE IF NOT EXISTS turso_ext").await;
    let e = mysql_server_error(&mut c, "CREATE DATABASE turso_ext").await;
    assert_eq!(e.code, ER_DB_CREATE_EXISTS, "{e:?}");

    // Read back over MySQL: the raw f32 blob decodes as a 2-dim
    // vector, the TEXT row round-trips untouched.
    c.query_drop("USE turso_ext").await.expect("use turso_ext");
    assert_eq!(
        rows(
            &mut c,
            "SELECT VECTOR_TO_STRING(v), VECTOR_DIM(v) FROM ext WHERE id = 1"
        )
        .await,
        vec![vec![s("[1,2]"), s("2")]]
    );
    assert_eq!(
        rows(&mut c, "SELECT v FROM ext WHERE id = 2").await,
        vec![vec![s("plain")]]
    );
}
