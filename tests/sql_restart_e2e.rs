//! kill -9 restart durability of the SQL MVCC clock: a single-machine
//! node (never `cluster init`-ed, so the LOCAL ts oracle is active)
//! must never hand out timestamps below rows it already acked. Before
//! the persisted `sql_ts_floor` key, a restart reset the oracle to 1:
//! every previously committed row became invisible (`commit_ts >
//! read_ts` for all snapshots) and a rewrite of the same pk was
//! shadowed by its own older, higher-ts version.
//!
//! Rows themselves were never lost (`ops::batch_write_async` fsyncs
//! before the ack), so this suite checks VISIBILITY across the restart:
//! multi-row autocommit INSERTs, an explicit BEGIN..COMMIT, a same-pk
//! UPDATE and a DELETE-then-INSERT of the same pk, on both a
//! single-column and a composite primary key table -- then, after the
//! restart, that fresh writes on the same pk are immediately visible
//! (the new clock sits strictly above every persisted version).

mod common;

use std::time::{Duration, Instant};

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";

async fn connect(node: &common::ProcNode) -> mysql_async::Conn {
    let port: u16 = node.mysql.rsplit(':').next().unwrap().parse().unwrap();
    let opts = || {
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .user(Some("root"))
            .pass(Some(PASS))
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => return c,
            Err(mysql_async::Error::Io(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

/// DDL needs the raft leader; retry until the bootstrap node becomes one.
async fn ddl(conn: &mut mysql_async::Conn, sql: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    continue;
                }
                panic!("ddl {sql}: {e}");
            }
        }
    }
}

async fn run(conn: &mut mysql_async::Conn, sql: &str) {
    conn.query_drop(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let rs: Vec<mysql_async::Row> = conn
        .query(sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect()
}

/// `(id, v)` pairs of the solo table, ordered by id.
async fn solo(conn: &mut mysql_async::Conn) -> Vec<(i64, String)> {
    rows(conn, "SELECT id, v FROM solo ORDER BY id")
        .await
        .iter()
        .map(|r| (m_int(&r[0]), m_str(&r[1])))
        .collect()
}

/// Text-protocol cells are length-prefixed bytes on the wire, so int
/// cells arrive as `Value::Bytes` too (see sql_txn_e2e).
fn m_int(v: &MVal) -> i64 {
    match v {
        MVal::Bytes(b) => std::str::from_utf8(b).unwrap().parse().unwrap(),
        MVal::Int(i) => *i,
        MVal::UInt(u) => *u as i64,
        other => panic!("expected int, got {other:?}"),
    }
}

fn m_str(v: &MVal) -> String {
    match v {
        MVal::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        other => panic!("expected string, got {other:?}"),
    }
}

#[tokio::test]
async fn kill9_restart_keeps_committed_rows_visible() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;

    ddl(
        &mut c,
        "CREATE TABLE solo (id BIGINT, v VARCHAR(64)) PRIMARY KEY(id)",
    )
    .await;
    ddl(
        &mut c,
        "CREATE TABLE comp (a INT, b VARCHAR(16), v BIGINT) PRIMARY KEY(a, b)",
    )
    .await;

    // 1. Autocommit multi-row INSERT (one statement, one ts range).
    run(
        &mut c,
        "INSERT INTO solo (id, v) VALUES (1, 'one'), (2, 'two'), (3, 'three')",
    )
    .await;
    // 2. Explicit BEGIN..COMMIT: inserts plus a same-pk UPDATE inside
    //    the txn (the commit batch stamps every version above the txn's
    //    read ts -- exactly the versions a rewound clock would hide).
    run(&mut c, "BEGIN").await;
    run(
        &mut c,
        "INSERT INTO solo (id, v) VALUES (10, 'ten'), (11, 'eleven')",
    )
    .await;
    run(&mut c, "UPDATE solo SET v = 'one-txn' WHERE id = 1").await;
    run(&mut c, "COMMIT").await;
    // 3. Autocommit same-pk UPDATE.
    run(&mut c, "UPDATE solo SET v = 'two-upd' WHERE id = 2").await;
    // 4. DELETE then re-INSERT the same pk (two versions, newest wins).
    run(&mut c, "DELETE FROM solo WHERE id = 3").await;
    run(
        &mut c,
        "INSERT INTO solo (id, v) VALUES (3, 'three-reborn')",
    )
    .await;
    // 5. Composite pk: insert, update one row's payload (same pk), and
    //    delete+reinsert one composite pk.
    run(
        &mut c,
        "INSERT INTO comp (a, b, v) VALUES (1, 'x', 100), (1, 'y', 200), (2, 'x', 300)",
    )
    .await;
    run(&mut c, "UPDATE comp SET v = 201 WHERE a = 1 AND b = 'y'").await;
    run(&mut c, "DELETE FROM comp WHERE a = 2 AND b = 'x'").await;
    run(&mut c, "INSERT INTO comp (a, b, v) VALUES (2, 'x', 301)").await;

    // Pre-kill sanity: everything visible before we take the process down.
    assert_eq!(
        solo(&mut c).await,
        vec![
            (1, "one-txn".to_string()),
            (2, "two-upd".to_string()),
            (3, "three-reborn".to_string()),
            (10, "ten".to_string()),
            (11, "eleven".to_string()),
        ]
    );

    // ---- kill -9, respawn on the SAME data dir + config ----
    node.kill_now();
    node.respawn();
    wait_resp_ready(&mut node, 30).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;

    // Every acked row is visible with its final value: the oracle's
    // clock resumed above every persisted version instead of at 1.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = solo(&mut c).await;
        if got.len() == 5 {
            assert_eq!(
                got,
                vec![
                    (1, "one-txn".to_string()),
                    (2, "two-upd".to_string()),
                    (3, "three-reborn".to_string()),
                    (10, "ten".to_string()),
                    (11, "eleven".to_string()),
                ],
                "post-restart visibility broken\n{}",
                node.ctx()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rows invisible after kill -9 restart\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(
        rows(&mut c, "SELECT a, b, v FROM comp ORDER BY a, b")
            .await
            .iter()
            .map(|r| (m_int(&r[0]), m_str(&r[1]), m_int(&r[2])))
            .collect::<Vec<_>>(),
        vec![
            (1, "x".to_string(), 100),
            (1, "y".to_string(), 201),
            (2, "x".to_string(), 301)
        ],
        "composite pk rows lost or shadowed after restart\n{}",
        node.ctx()
    );

    // Post-restart writes on the SAME pk must be immediately visible
    // and not shadowed by the pre-restart versions.
    run(&mut c, "UPDATE solo SET v = 'post-restart' WHERE id = 1").await;
    run(&mut c, "DELETE FROM solo WHERE id = 2").await;
    run(
        &mut c,
        "INSERT INTO solo (id, v) VALUES (2, 'reborn-again')",
    )
    .await;
    run(&mut c, "INSERT INTO solo (id, v) VALUES (12, 'fresh')").await;
    assert_eq!(
        solo(&mut c).await,
        vec![
            (1, "post-restart".to_string()),
            (2, "reborn-again".to_string()),
            (3, "three-reborn".to_string()),
            (10, "ten".to_string()),
            (11, "eleven".to_string()),
            (12, "fresh".to_string()),
        ],
        "post-restart same-pk writes shadowed\n{}",
        node.ctx()
    );
}

/// `(id, v)` pairs of this suite's `t` table, ordered by id.
async fn t_rows(c: &mut mysql_async::Conn) -> Vec<(i64, String)> {
    rows(c, "SELECT id, v FROM t ORDER BY id")
        .await
        .iter()
        .map(|r| (m_int(&r[0]), m_str(&r[1])))
        .collect()
}

/// In-place upgrade from a pre-floor binary: the store holds committed
/// rows but NO `sql_ts_floor` key (old binaries never stamped one). A
/// naive boot reports floor 0, the oracle restarts at 1 and every row
/// goes invisible -- the boot path must instead recover the clock by
/// SCANNING the store's own versions, stamp the key so the scan is
/// strictly one-shot, and keep the data visible across every restart
/// after that.
#[tokio::test]
async fn missing_floor_key_boot_scan_recovers_the_clock() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-floorscan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 30).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;

    ddl(
        &mut c,
        "CREATE TABLE t (id BIGINT, v VARCHAR(64)) PRIMARY KEY(id)",
    )
    .await;
    run(
        &mut c,
        "INSERT INTO t (id, v) VALUES (1, 'one'), (2, 'two')",
    )
    .await;
    run(&mut c, "UPDATE t SET v = 'one-upd' WHERE id = 1").await;
    assert_eq!(
        t_rows(&mut c).await,
        vec![(1, "one-upd".to_string()), (2, "two".to_string())]
    );

    // ---- stop, then strip the floor key straight out of the store ----
    node.kill_now();
    let store_dir = node.dir.join(&node.resp); // data_path(store_path, bind)
    assert!(
        store_dir.join("CURRENT").is_file(),
        "expected a RocksDB store at {}",
        store_dir.display()
    );
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = rocksdb::DB::open(&opts, store_dir.to_str().unwrap()).expect("open store");
        let mut wo = rocksdb::WriteOptions::default();
        wo.set_sync(true);
        let mut batch = rocksdb::WriteBatch::default();
        batch.delete(rdb::sql::tx::floor::FLOOR_KEY);
        db.write_opt(batch, &wo).expect("delete floor key");
        assert!(
            db.get(rdb::sql::tx::floor::FLOOR_KEY)
                .expect("get floor key")
                .is_none(),
            "floor key must be gone before the restart"
        );
    } // DB handle dropped: the lock is released for the respawn

    // ---- first post-"upgrade" boot: the scan must fence the clock ----
    node.respawn();
    wait_resp_ready(&mut node, 30).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = t_rows(&mut c).await;
        if got.len() == 2 {
            assert_eq!(
                got,
                vec![(1, "one-upd".to_string()), (2, "two".to_string())],
                "rows invisible after floor-less upgrade boot\n{}",
                node.ctx()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "rows invisible after floor-less upgrade boot\n{}",
            node.ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // Same-pk rewrite must land on top, not be shadowed.
    run(&mut c, "UPDATE t SET v = 'post-scan' WHERE id = 1").await;
    assert_eq!(
        t_rows(&mut c).await,
        vec![(1, "post-scan".to_string()), (2, "two".to_string())]
    );

    // The scan branch must have STAMPED the floor key: kill before any
    // further write dependencies and read the key back directly.
    node.kill_now();
    {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        let db = rocksdb::DB::open(&opts, store_dir.to_str().unwrap()).expect("reopen store");
        let raw = db
            .get(rdb::sql::tx::floor::FLOOR_KEY)
            .expect("get floor key")
            .expect("scan branch must stamp the floor key");
        let stamped = u64::from_be_bytes(raw[..8].try_into().expect("8-byte floor"));
        assert!(
            stamped >= 3,
            "stamped floor {stamped} must cover 3 versions"
        );
    }

    // ---- second boot: the key exists now, so no scan must run again ----
    node.respawn();
    wait_resp_ready(&mut node, 30).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = connect(&node).await;
    assert_eq!(
        t_rows(&mut c).await,
        vec![(1, "post-scan".to_string()), (2, "two".to_string())],
        "rows lost across the scan-free second boot\n{}",
        node.ctx()
    );
    run(&mut c, "INSERT INTO t (id, v) VALUES (3, 'third')").await;
    assert_eq!(
        t_rows(&mut c).await,
        vec![
            (1, "post-scan".to_string()),
            (2, "two".to_string()),
            (3, "third".to_string()),
        ]
    );

    // Exactly ONE scan line across all three boots of this stderr log.
    node.kill_now();
    let log = std::fs::read_to_string(&node.stderr_path).expect("read stderr log");
    let scans = log.matches("sql ts: boot floor scan recovered").count();
    assert_eq!(scans, 1, "boot scan must be strictly one-shot\n{log}");
    let _ = std::fs::remove_dir_all(&dir);
}
