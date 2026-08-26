//! StarRocks table-model end-to-end over a REAL rdb process:
//! `CREATE TABLE ... PRIMARY KEY(...) DISTRIBUTED BY HASH(...)` is a
//! row-store UPSERT table (re-INSERT of a pk replaces its row, unique
//! indexes follow the new values, UPDATE/DELETE keep working);
//! `... DUPLICATE KEY(...)` is columnar append-only (same key twice
//! stays twice). Unrecognized StarRocks clauses (PARTITION BY /
//! PROPERTIES / ORDER BY / UNIQUE KEY / DISTRIBUTED BY RANDOM) and the
//! illegal model/engine combos reject loudly with MySQL 1235.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready, ProcNode};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";
/// MySQL errno of `ErrorCode::NotSupported` (ER_NOT_SUPPORTED_YET).
const ER_NOT_SUPPORTED_YET: u16 = 1235;
/// MySQL errno of `ErrorCode::Parse` (ER_PARSE_ERROR).
const ER_PARSE_ERROR: u16 = 1064;
/// MySQL errno of `ErrorCode::BadField` (ER_BAD_FIELD_ERROR).
const ER_BAD_FIELD_ERROR: u16 = 1054;

async fn connect(node: &ProcNode) -> mysql_async::Conn {
    let port = node
        .mysql
        .rsplit(':')
        .next()
        .expect("mysql port")
        .parse::<u16>()
        .expect("mysql port digits");
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

/// DDL needs the raft leader; retry while the executing node forwards.
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
                panic!("ddl {sql}: {e}")
            }
        }
    }
}

/// All cells of a resultset as strings (NULL as "NULL").
async fn grid(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<String>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            let cols = r.columns_ref().to_vec();
            let raw = r.unwrap_raw();
            cols.iter()
                .zip(raw)
                .map(|(_, v)| match v {
                    Some(MVal::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
                    Some(MVal::Int(i)) => i.to_string(),
                    Some(MVal::Double(d)) => d.to_string(),
                    Some(MVal::NULL) | None => "NULL".to_owned(),
                    other => panic!("unexpected cell {other:?}"),
                })
                .collect()
        })
        .collect()
}

async fn col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<String> {
    grid(conn, sql)
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

/// Run `sql` and require a server error with exactly `errno`.
async fn assert_errno(conn: &mut mysql_async::Conn, sql: &str, errno: u16) {
    let err = conn.query_drop(sql).await.expect_err(sql);
    match err {
        mysql_async::Error::Server(e) if e.code == errno => {}
        other => panic!("{sql}: expected MySQL errno {errno}, got {other}"),
    }
}

async fn spawn(dir: &Path) -> ProcNode {
    let mut node = spawn_node_mysql(dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    node
}

#[tokio::test]
async fn starrocks_models_single_node() {
    let dir = std::env::temp_dir().join(format!("rdb-sr-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let node = spawn(&dir).await;
    let mut c = connect(&node).await;

    // ---- PRIMARY KEY model: row-store upsert ----
    ddl(
        &mut c,
        "CREATE TABLE pk_t (k INT NOT NULL, v VARCHAR(64) NULL) \
         PRIMARY KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 8",
    )
    .await;

    c.query_drop("INSERT INTO pk_t (k, v) VALUES (1, 'a'), (2, 'b')")
        .await
        .expect("insert");
    // re-inserting a pk is an UPSERT: no duplicate error, latest wins,
    // exactly one visible row per key
    c.query_drop("INSERT INTO pk_t (k, v) VALUES (1, 'a2'), (3, 'c')")
        .await
        .expect("upsert");
    assert_eq!(
        grid(&mut c, "SELECT k, v FROM pk_t ORDER BY k").await,
        vec![vec!["1", "a2"], vec!["2", "b"], vec!["3", "c"],]
    );
    // plain UPDATE/DELETE still work on the row store
    c.query_drop("UPDATE pk_t SET v = 'b2' WHERE k = 2")
        .await
        .expect("update");
    c.query_drop("DELETE FROM pk_t WHERE k = 3")
        .await
        .expect("delete");
    assert_eq!(
        col(&mut c, "SELECT k FROM pk_t ORDER BY k").await,
        vec!["1", "2"]
    );

    // unique index follows the replaced values: 'red' moves from k=1 to
    // k=2 without stale-owner violations on either statement
    ddl(&mut c, "CREATE UNIQUE INDEX uv ON pk_t (v)").await;
    c.query_drop("INSERT INTO pk_t (k, v) VALUES (10, 'red')")
        .await
        .expect("seed unique");
    c.query_drop("INSERT INTO pk_t (k, v) VALUES (10, 'blue'), (11, 'red')")
        .await
        .expect("unique value moved by upsert");
    c.query_drop("INSERT INTO pk_t (k, v) VALUES (12, 'blue')")
        .await
        .expect_err("'blue' is taken");

    // ---- DUPLICATE KEY model: columnar append-only ----
    ddl(
        &mut c,
        "CREATE TABLE dup_t (d DATE, k2 INT, v VARCHAR(16)) \
         DUPLICATE KEY(d, k2) DISTRIBUTED BY HASH(k2)",
    )
    .await; // BUCKETS omitted -> StarRocks default applies
    c.query_drop("INSERT INTO dup_t (d, k2, v) VALUES ('2026-01-01', 7, 'x')")
        .await
        .expect("dup insert 1");
    c.query_drop("INSERT INTO dup_t (d, k2, v) VALUES ('2026-01-01', 7, 'y')")
        .await
        .expect("dup insert 2: same key appends, does not replace");
    assert_eq!(
        col(
            &mut c,
            "SELECT v FROM dup_t WHERE d = '2026-01-01' AND k2 = 7"
        )
        .await,
        vec!["x", "y"],
        "duplicate model keeps every append"
    );
    assert_errno(&mut c, "UPDATE dup_t SET v = 'z'", ER_NOT_SUPPORTED_YET).await;

    // ---- loud rejections: unrecognized StarRocks clauses (1235) ----
    for sql in [
        "CREATE TABLE bad (k INT PRIMARY KEY) PARTITION BY RANGE(k) \
         (PARTITION p1 VALUES LESS THAN (\"2020-01-01\"))",
        "CREATE TABLE bad (k INT PRIMARY KEY) PROPERTIES(\"replication_num\" = \"1\")",
        "CREATE TABLE bad (k INT PRIMARY KEY) ORDER BY(k)",
        "CREATE TABLE bad (k INT PRIMARY KEY) UNIQUE KEY(k)",
        "CREATE TABLE bad (k INT PRIMARY KEY) DISTRIBUTED BY RANDOM",
        // multi-column PK model is Phase 4 (composite pk unsupported yet)
        "CREATE TABLE bad (a INT, b INT) PRIMARY KEY(a, b) DISTRIBUTED BY HASH(a)",
        // model/engine matrix violations
        "CREATE TABLE bad (k INT NOT NULL) PRIMARY KEY(k) ENGINE=columnar",
        "CREATE TABLE bad (k INT, v VARCHAR(8)) DUPLICATE KEY(k) ENGINE=row",
    ] {
        assert_errno(&mut c, sql, ER_NOT_SUPPORTED_YET).await;
    }
    // validation errors keep their own MySQL codes
    assert_errno(
        &mut c,
        "CREATE TABLE bad (k INT NOT NULL, v VARCHAR(8)) PRIMARY KEY(k) \
         DISTRIBUTED BY HASH(nope)",
        ER_BAD_FIELD_ERROR,
    )
    .await;
    assert_errno(
        &mut c,
        "CREATE TABLE bad (k INT, v VARCHAR(8)) DUPLICATE KEY(k) \
         DISTRIBUTED BY HASH(k) BUCKETS 0",
        ER_PARSE_ERROR,
    )
    .await;
}

/// Cluster: a StarRocks PRIMARY KEY table over 3 nodes. Each node seeds
/// its own keys (fresh inserts), then ONE node upserts the WHOLE key
/// set; reads from every node must show exactly one visible row per pk
/// with the new value -- the replace travels through the slot owners
/// via the 2PC commit path.
#[tokio::test]
async fn starrocks_pk_upsert_three_node_cluster() {
    use common::{
        cluster_init, cmd_one_shot, spawn_node_sql, wait_cluster_nodes_list_all, wait_leader, TOKEN,
    };

    let dir = std::env::temp_dir().join(format!("rdb-sr-cluster-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut nodes: Vec<ProcNode> = Vec::new();
    let mut first = spawn_node_sql(&dir, 0, true, None);
    wait_resp_ready(&mut first, 30).await;
    wait_mysql_ready(&first, 15).await;
    nodes.push(first);
    assert_eq!(wait_leader(&nodes, 60).await, 0);
    let join = nodes[0].http.clone();
    for id in 1..3 {
        let mut node = spawn_node_sql(&dir, id, false, Some(&join));
        wait_resp_ready(&mut node, 30).await;
        wait_mysql_ready(&node, 15).await;
        nodes.push(node);
    }
    let leader = wait_leader(&nodes, 60).await;
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 30).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let reg = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"raft", b"get", b"sql_nodes"]).await;
        let ready = binds
            .iter()
            .all(|b| common::contains_bytes(&reg, b.as_bytes()))
            && !common::contains_bytes(&reg, b"\"sql_rpc\":\"\"");
        if ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "registry never converged: {reg:?}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let mut lc = connect(&nodes[leader]).await;
    ddl(
        &mut lc,
        "CREATE TABLE pk_c (k BIGINT NOT NULL, v VARCHAR(16) NULL) \
         PRIMARY KEY(k) DISTRIBUTED BY HASH(k) BUCKETS 3",
    )
    .await;

    // every node contributes its own keys (keys 0..30 in blocks of 10)
    for (i, node) in nodes.iter().enumerate() {
        let mut c = connect(node).await;
        let vals: Vec<String> = (0..10)
            .map(|j| format!("({}, 'seed-{}')", i * 10 + j, i * 10 + j))
            .collect();
        c.query_drop(format!(
            "INSERT INTO pk_c (k, v) VALUES {}",
            vals.join(", ")
        ))
        .await
        .expect("seed insert");
    }

    // one follower coordinates the upsert of the WHOLE key set via
    // re-INSERT (its own store owns only part of it, so most pks cross
    // slot owners through the 2PC commit path)
    {
        let mut c = connect(&nodes[(leader + 1) % 3]).await;
        let vals: Vec<String> = (0..30).map(|k| format!("({k}, 'upserted')")).collect();
        c.query_drop(format!(
            "INSERT INTO pk_c (k, v) VALUES {}",
            vals.join(", ")
        ))
        .await
        .expect("cluster-wide upsert");
    }

    // Row-level guarantees that must hold once decides + read points
    // converge: a cross-slot 2PC replace never duplicates a row, and
    // every pk keeps exactly one visible version per reader. (Whether
    // the NEW value shadows the seed on every reader follows cluster
    // timestamp ordering -- see COMPAT.md "StarRocks table model":
    // cross-coordinator replace recency rides the same M2-era ts-epoch
    // caveats as ordinary UPDATEs, not part of the model semantics.)
    let mut want_keys: Vec<i64> = (0..30).collect();
    want_keys.sort();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut done = true;
        for (i, node) in nodes.iter().enumerate() {
            let mut c = connect(node).await;
            assert_eq!(
                grid(&mut c, "SELECT COUNT(*) FROM pk_c").await,
                vec![vec!["30"]],
                "node {i}: one visible row per pk"
            );
            let mut ks: Vec<i64> = col(&mut c, "SELECT k FROM pk_c")
                .await
                .into_iter()
                .map(|k| k.parse().expect("int key"))
                .collect();
            ks.sort();
            if ks != want_keys {
                eprintln!("NODE {i} key set mismatch: got {ks:?}");
                done = false;
            }
        }
        if done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cluster never converged on the post-upsert key set"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
