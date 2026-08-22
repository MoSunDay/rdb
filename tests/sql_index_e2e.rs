//! Secondary/unique indexes plus the access-path planner over a real
//! rdb process: index lookups (eq/IN/BETWEEN), EXPLAIN verdicts, entry
//! maintenance under UPDATE/DELETE, unique rejection + reclaim, txn
//! overlay at COMMIT, DROP INDEX sweeps and the wide-scan fallback.

mod common;

use common::{spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::{prelude::*, OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";
async fn connect(node: &common::ProcNode) -> mysql_async::Conn {
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match mysql_async::Conn::new(opts()).await {
            Ok(c) => return c,
            Err(mysql_async::Error::Io(_)) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await
            }
            Err(e) => panic!("mysql connect: {e}"),
        }
    }
}

/// DDL needs the raft leader; retry until the bootstrap node becomes one.
async fn ddl(conn: &mut mysql_async::Conn, sql: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match conn.query_drop(sql).await {
            Ok(()) => return,
            Err(e) => {
                if std::time::Instant::now() < deadline && e.to_string().contains("leader") {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
                panic!("ddl {sql}: {e}");
            }
        }
    }
}

/// One fresh single-node world: node up + table created (no indexes).
async fn world(name: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-idx-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut conn = connect(&node).await;
    ddl(
        &mut conn,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VARCHAR(64) NULL, n BIGINT NULL)",
    )
    .await;
    (node, conn)
}

async fn rows(conn: &mut mysql_async::Conn, sql: &str) -> Vec<Vec<MVal>> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    rs.into_iter()
        .map(|r| {
            (0..r.len())
                .map(|i| r.get::<MVal, _>(i).unwrap_or(MVal::NULL))
                .collect()
        })
        .collect()
}

/// Text-protocol cells are always `Value::Bytes` (length-prefixed on
/// the wire) -- compare raw bytes, never Debug output.
fn int(i: i64) -> MVal {
    MVal::Bytes(i.to_string().into_bytes())
}
fn s(v: &str) -> MVal {
    MVal::Bytes(v.as_bytes().to_vec())
}
fn ids(rs: &[Vec<MVal>]) -> Vec<MVal> {
    let mut out: Vec<MVal> = rs.iter().map(|r| r[0].clone()).collect();
    out.sort_by(cmp_mval);
    out
}
fn cmp_mval(a: &MVal, b: &MVal) -> std::cmp::Ordering {
    let num = |v: &MVal| -> Option<i64> {
        match v {
            MVal::Int(i) => Some(*i),
            MVal::Bytes(b) => std::str::from_utf8(b).ok()?.parse().ok(),
            _ => None,
        }
    };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => match (a, b) {
            (MVal::Bytes(x), MVal::Bytes(y)) => x.cmp(y),
            _ => std::cmp::Ordering::Equal,
        },
    }
}
async fn exec(conn: &mut mysql_async::Conn, sql: &str) -> String {
    match conn.query_drop(sql).await {
        Ok(()) => String::new(),
        Err(e) => e.to_string(),
    }
}
async fn plan_line(conn: &mut mysql_async::Conn, sql: &str) -> String {
    let rs = rows(conn, sql).await;
    assert!(!rs.is_empty(), "explain returned nothing");
    match &rs[0][0] {
        MVal::Bytes(b) => String::from_utf8(b.clone()).unwrap(),
        other => panic!("plan line {other:?}"),
    }
}
#[tokio::test]
async fn secondary_index_lookup_and_explain() {
    let (mut node, mut c) = world("lookup").await;
    ddl(&mut c, "CREATE INDEX idx_v ON t (v)").await;
    for (id, v) in [(1, "red"), (2, "red"), (3, "blue")] {
        c.query_drop(format!("INSERT INTO t (id, v) VALUES ({id}, '{v}')"))
            .await
            .unwrap();
    }

    // eq through the index; residual WHERE hides nothing extra here
    let got = ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'red'").await);
    assert_eq!(got, vec![int(1), int(2)]);
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t WHERE v = 'red'").await,
        "IndexScan idx_v -> 2 pks"
    );

    // IN = one point lookup per literal
    let got = ids(&rows(&mut c, "SELECT id FROM t WHERE v IN ('blue', 'zzz')").await);
    assert_eq!(got, vec![int(3)]);

    // BETWEEN covers a value range (blue..red inclusive)
    let got = ids(&rows(&mut c, "SELECT id FROM t WHERE v BETWEEN 'blue' AND 'red'").await);
    assert_eq!(got, vec![int(1), int(2), int(3)]);

    // no usable conjunct -> plain scan headline
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t").await,
        "SeqScan t"
    );
    node.kill_now();
}

#[tokio::test]
async fn update_and_delete_maintain_entries() {
    let (mut node, mut c) = world("maintain").await;
    ddl(&mut c, "CREATE INDEX idx_v ON t (v)").await;
    c.query_drop("INSERT INTO t (id, v) VALUES (1, 'red'), (2, 'blue')")
        .await
        .unwrap();

    // UPDATE: the stale entry must vanish, the new one appear
    c.query_drop("UPDATE t SET v = 'green' WHERE id = 1")
        .await
        .unwrap();
    assert_eq!(
        ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'red'").await),
        Vec::<MVal>::new()
    );
    assert_eq!(
        ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'green'").await),
        vec![int(1)]
    );

    // DELETE removes the entries with the row
    c.query_drop("DELETE FROM t WHERE id = 2").await.unwrap();
    assert_eq!(
        ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'blue'").await),
        Vec::<MVal>::new()
    );
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t WHERE v = 'blue'").await,
        "IndexScan idx_v -> 0 pks",
        "an emptied index is still the chosen path"
    );
    node.kill_now();
}

#[tokio::test]
async fn unique_index_rejects_and_reclaims() {
    let (mut node, mut c) = world("unique").await;
    ddl(&mut c, "CREATE UNIQUE INDEX uq_n ON t (n)").await;
    c.query_drop("INSERT INTO t (id, n) VALUES (1, 10)")
        .await
        .unwrap();

    // different pk claiming an owned value -> duplicate entry
    let err = exec(&mut c, "INSERT INTO t (id, n) VALUES (2, 10)").await;
    assert!(
        err.contains("Duplicate entry") && err.contains("uq_n"),
        "{err}"
    );
    // the rejected row never landed
    assert_eq!(
        rows(&mut c, "SELECT id FROM t WHERE id = 2").await,
        Vec::<Vec<MVal>>::new()
    );

    // intra-batch clash is rejected as one statement
    let err = exec(&mut c, "INSERT INTO t (id, n) VALUES (3, 30), (4, 30)").await;
    assert!(err.contains("Duplicate entry"), "{err}");

    // NULL never conflicts (NULL is never indexed)
    c.query_drop("INSERT INTO t (id, n) VALUES (5, NULL), (6, NULL)")
        .await
        .unwrap();

    // deleting the owner frees the value
    c.query_drop("DELETE FROM t WHERE id = 1").await.unwrap();
    c.query_drop("INSERT INTO t (id, n) VALUES (2, 10)")
        .await
        .unwrap();

    // rewriting a row with its own value stays legal (pk keeps
    // ownership); a genuine clash with another owner is rejected
    c.query_drop("INSERT INTO t (id, n) VALUES (7, 70)")
        .await
        .unwrap();
    c.query_drop("UPDATE t SET n = 10 WHERE id = 2")
        .await
        .unwrap();
    let err = exec(&mut c, "UPDATE t SET n = 70 WHERE id = 2").await;
    assert!(err.contains("Duplicate entry"), "{err}");
    assert_eq!(
        ids(&rows(&mut c, "SELECT id FROM t WHERE n = 10").await),
        vec![int(2)]
    );
    node.kill_now();
}

#[tokio::test]
async fn create_unique_index_rejects_existing_duplicates() {
    let (mut node, mut c) = world("create-uq").await;
    c.query_drop("INSERT INTO t (id, n) VALUES (1, 5), (2, 5)")
        .await
        .unwrap();
    let err = exec(&mut c, "CREATE UNIQUE INDEX uq_n ON t (n)").await;
    assert!(err.contains("Duplicate entry"), "{err}");
    // the failed DDL left no index behind: reads plan as SeqScan
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t WHERE n = 5").await,
        "SeqScan t"
    );
    node.kill_now();
}

#[tokio::test]
async fn txn_overlay_and_commit_maintenance() {
    let (mut node, mut c) = world("txn").await;
    ddl(&mut c, "CREATE INDEX idx_v ON t (v)").await;
    let mut b = connect(&node).await;
    c.query_drop("INSERT INTO t (id, v) VALUES (1, 'red')")
        .await
        .unwrap();

    // staged insert is visible through the overlay even though its
    // index entry does not exist yet (the index found only pk 1)
    c.query_drop("BEGIN").await.unwrap();
    c.query_drop("INSERT INTO t (id, v) VALUES (2, 'red')")
        .await
        .unwrap();
    let got = ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'red'").await);
    assert_eq!(got, vec![int(1), int(2)]);

    // staged delete hides the fetched pk
    c.query_drop("DELETE FROM t WHERE id = 1").await.unwrap();
    let got = ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'red'").await);
    assert_eq!(got, vec![int(2)]);

    // a second connection sees only the committed state
    assert_eq!(
        ids(&rows(&mut b, "SELECT id FROM t WHERE v = 'red'").await),
        vec![int(1)]
    );

    c.query_drop("COMMIT").await.unwrap();
    // after COMMIT the entries are maintained: fresh reader agrees
    let got = ids(&rows(&mut b, "SELECT id FROM t WHERE v = 'red'").await);
    assert_eq!(got, vec![int(2)]);

    // ROLLBACK: nothing staged ever reaches the index
    c.query_drop("BEGIN").await.unwrap();
    c.query_drop("INSERT INTO t (id, v) VALUES (3, 'red')")
        .await
        .unwrap();
    c.query_drop("ROLLBACK").await.unwrap();
    let got = ids(&rows(&mut b, "SELECT id FROM t WHERE v = 'red'").await);
    assert_eq!(got, vec![int(2)]);
    node.kill_now();
}

#[tokio::test]
async fn unique_violation_at_commit() {
    let (mut node, mut c) = world("txn-uq").await;
    ddl(&mut c, "CREATE UNIQUE INDEX uq_n ON t (n)").await;
    let mut b = connect(&node).await;
    c.query_drop("INSERT INTO t (id, n) VALUES (1, 10)")
        .await
        .unwrap();

    // B stages the same unique value before A commits it
    b.query_drop("BEGIN").await.unwrap();
    b.query_drop("INSERT INTO t (id, n) VALUES (2, 10)")
        .await
        .unwrap();
    c.query_drop("INSERT INTO t (id, n) VALUES (3, 30)")
        .await
        .unwrap();

    let err = exec(&mut b, "COMMIT").await;
    assert!(
        err.contains("Duplicate entry") && err.contains("uq_n"),
        "{err}"
    );
    assert_eq!(
        rows(&mut b, "SELECT id FROM t WHERE id = 2").await,
        Vec::<Vec<MVal>>::new(),
        "the rejected txn wrote nothing"
    );
    node.kill_now();
}

#[tokio::test]
async fn drop_index_sweeps_and_planner_falls_back() {
    let (mut node, mut c) = world("drop").await;
    ddl(&mut c, "CREATE INDEX idx_v ON t (v)").await;
    ddl(&mut c, "CREATE UNIQUE INDEX uq_n ON t (n)").await;
    c.query_drop("INSERT INTO t (id, v, n) VALUES (1, 'red', 10), (2, 'blue', 20)")
        .await
        .unwrap();

    // SHOW INDEX lists both, pk first
    let shown = rows(&mut c, "SHOW INDEX FROM t").await;
    assert_eq!(shown.len(), 3);
    assert_eq!(shown[0][2], s("PRIMARY"));
    assert_eq!(shown[1][2], s("idx_v"));
    assert_eq!(shown[2][2], s("uq_n"));

    ddl(&mut c, "DROP INDEX idx_v ON t").await;
    // the secondary lookups degrade to seq scans, unique stays live
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t WHERE v = 'red'").await,
        "SeqScan t"
    );
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t WHERE n = 10").await,
        "IndexScan uq_n -> 1 pks"
    );
    // rows survive the entry sweep untouched
    assert_eq!(rows(&mut c, "SELECT id FROM t").await.len(), 2);
    node.kill_now();
}

#[tokio::test]
async fn wide_index_result_falls_back_to_seqscan() {
    let (mut node, mut c) = world("wide").await;
    ddl(&mut c, "CREATE INDEX idx_v ON t (v)").await;
    for chunk in 0..11 {
        let mut sql = String::from("INSERT INTO t (id, v) VALUES ");
        for i in 0..100 {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({}, 'same')", chunk * 100 + i));
        }
        c.query_drop(sql).await.unwrap();
    }
    // 1100 matching pks > MAX_INDEX_PKS: the planner prefers a scan
    assert_eq!(
        plan_line(&mut c, "EXPLAIN SELECT id FROM t WHERE v = 'same'").await,
        "SeqScan t"
    );
    let got = ids(&rows(&mut c, "SELECT id FROM t WHERE v = 'same'").await);
    assert_eq!(got.len(), 1100);
    node.kill_now();
}
