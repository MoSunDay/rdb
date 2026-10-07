//! M0 query-semantics e2e over a real rdb MySQL-protocol process (see
//! `plans/2026-10-06-mysql-gap/m0-query-semantics.md`): bare-integer
//! ORDER BY / GROUP BY ordinals (vs '1'/-1/1+1 constants), select-list
//! alias resolution in ORDER BY / HAVING, LIMIT ? / OFFSET ? over the
//! binary prepared-statement path, and FROM DUAL == no FROM.

mod common;

use common::{mysql_root_conn, spawn_node_mysql, wait_mysql_ready, wait_resp_ready};
use mysql_async::prelude::*;
use mysql_async::Value as MVal;

const PASS: &str = "e2e-sql-pass";
/// MySQL errno of `ErrorCode::BadField` (ER_BAD_FIELD_ERROR).
const ER_BAD_FIELD_ERROR: u16 = 1054;
/// MySQL errno of `ErrorCode::NotSupported` (ER_NOT_SUPPORTED_YET).
const ER_NOT_SUPPORTED_YET: u16 = 1235;
/// MySQL errno of `ErrorCode::Parse` (ER_PARSE_ERROR).
const ER_PARSE_ERROR: u16 = 1064;
/// MySQL errno of `ErrorCode::NoSuchTable` (ER_NO_SUCH_TABLE).
const ER_NO_SUCH_TABLE: u16 = 1146;

/// DDL needs the raft leader; the bootstrap node becomes one within a
/// second or two, so retry the first CREATE until it sticks.
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
                panic!("ddl {sql}: {e}")
            }
        }
    }
}

/// Rows as raw wire cells (text protocol hands ints back as Bytes).
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

/// First output column of every text-protocol row, decoded as i64.
async fn col(conn: &mut mysql_async::Conn, sql: &str) -> Vec<i64> {
    rows(conn, sql)
        .await
        .into_iter()
        .map(|r| match &r[0] {
            MVal::Bytes(b) => String::from_utf8_lossy(b).parse::<i64>().unwrap(),
            other => panic!("expected int cell, got {other:?}"),
        })
        .collect()
}

fn int(i: i64) -> MVal {
    MVal::Bytes(i.to_string().into_bytes())
}

/// Numeric sort key of an all-int text row: GROUP BY output order is
/// first-appearance, so row-set comparisons sort both sides first.
fn int_key(row: &[MVal]) -> Vec<i64> {
    row.iter()
        .map(|c| match c {
            MVal::Bytes(b) => String::from_utf8_lossy(b).parse::<i64>().expect("int cell"),
            other => panic!("expected int cell, got {other:?}"),
        })
        .collect()
}

/// The server error a text-protocol query failed with.
async fn server_error(conn: &mut mysql_async::Conn, sql: &str) -> mysql_async::ServerError {
    match conn.query::<mysql_async::Row, _>(sql).await {
        Err(mysql_async::Error::Server(e)) => e,
        other => panic!("expected server error for {sql}, got {other:?}"),
    }
}

/// The server error PREPARE failed with (M0 rejects some shapes at
/// translate time, which is inside parse_statement).
async fn prepare_error(conn: &mut mysql_async::Conn, sql: &str) -> mysql_async::ServerError {
    match conn.prep(sql).await {
        Err(mysql_async::Error::Server(e)) => e,
        other => panic!("expected server error preparing {sql}, got {other:?}"),
    }
}

/// One fresh bootstrap node with the SQL plane up, plus a shuffled
/// table where `a` grows with id and `b` shrinks: ordering by one
/// column differs from the other and from any plausible scan order.
async fn qsem_node(tag: &str) -> (common::ProcNode, mysql_async::Conn) {
    let dir = std::env::temp_dir().join(format!("rdb-sql-qsem-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut node = spawn_node_mysql(&dir, 0, true, None);
    wait_resp_ready(&mut node, 15).await;
    wait_mysql_ready(&node, 15).await;
    let mut c = mysql_root_conn(&node, PASS).await;
    ddl(
        &mut c,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a BIGINT NOT NULL, b BIGINT NOT NULL)",
    )
    .await;
    c.query_drop("INSERT INTO t (id, a, b) VALUES (3, 30, 2), (1, 10, 4), (4, 40, 1), (2, 20, 3)")
        .await
        .expect("seed t");
    (node, c)
}

#[tokio::test]
async fn order_by_ordinal_sorts_output_columns() {
    let (mut node, mut c) = qsem_node("ordinal").await;

    // ORDER BY 2 == ORDER BY b: ids by b asc (id4=1..id1=4), not id scan order
    let by_ordinal = col(&mut c, "SELECT id, b FROM t ORDER BY 2").await;
    let by_name = col(&mut c, "SELECT id, b FROM t ORDER BY b").await;
    assert_eq!(by_ordinal, by_name, "ORDER BY 2 must mean ORDER BY b");
    assert_eq!(by_ordinal, vec![4, 3, 2, 1]);

    // ordinal over an expression projection sorts by its VALUE
    let got = col(&mut c, "SELECT b * 10 FROM t ORDER BY 1").await;
    assert_eq!(got, vec![10, 20, 30, 40]);
    let got = col(&mut c, "SELECT b * 10 FROM t ORDER BY 1 DESC").await;
    assert_eq!(got, vec![40, 30, 20, 10]);

    // non-bare numbers stay constants: the key never changes, so all
    // three queries return the untouched scan order -- which (b is not
    // stored sorted) differs from the ordinal results above.
    let noop_str = col(&mut c, "SELECT b * 10 FROM t ORDER BY '1'").await;
    let noop_neg = col(&mut c, "SELECT b * 10 FROM t ORDER BY -1").await;
    let noop_sum = col(&mut c, "SELECT b * 10 FROM t ORDER BY 1 + 1").await;
    assert_eq!(
        noop_str, noop_neg,
        "quoted and signed numbers are constants"
    );
    assert_eq!(noop_str, noop_sum, "1+1 is composed, not a position");
    assert_ne!(
        noop_str,
        vec![10, 20, 30, 40],
        "a constant key must not sort anything"
    );

    node.kill_now();
}

#[tokio::test]
async fn group_by_ordinal_groups_by_output_position() {
    let (mut node, mut c) = qsem_node("group").await;
    ddl(
        &mut c,
        "CREATE TABLE g (id BIGINT PRIMARY KEY, tag BIGINT NOT NULL)",
    )
    .await;
    c.query_drop("INSERT INTO g (id, tag) VALUES (2, 10), (1, 10), (4, 20), (3, 10), (5, 20)")
        .await
        .expect("seed g");

    // GROUP BY 2 == GROUP BY tag (second output column), sorted for comparison
    let mut by_ordinal = rows(&mut c, "SELECT COUNT(*) AS cnt, tag FROM g GROUP BY 2").await;
    let mut by_name = rows(&mut c, "SELECT COUNT(*) AS cnt, tag FROM g GROUP BY tag").await;
    by_ordinal.sort_by_key(|r| int_key(r));
    by_name.sort_by_key(|r| int_key(r));
    assert_eq!(by_ordinal, by_name, "GROUP BY 2 must mean GROUP BY tag");
    assert_eq!(
        by_ordinal,
        vec![vec![int(2), int(20)], vec![int(3), int(10)]]
    );

    // ordinal over an expression projection groups by its value
    let mut got = rows(
        &mut c,
        "SELECT tag * 10 AS t10, COUNT(*) AS c FROM g GROUP BY 1",
    )
    .await;
    got.sort_by_key(|r| int_key(r));
    assert_eq!(got, vec![vec![int(100), int(3)], vec![int(200), int(2)]]);

    // out-of-range ordinal is MySQL's ER 1054 group-clause wording
    let e = server_error(&mut c, "SELECT id FROM g GROUP BY 9").await;
    assert_eq!(e.code, ER_BAD_FIELD_ERROR, "{}", e.message);
    assert!(
        e.message
            .contains("Unknown column '9' in 'group statement'"),
        "{}",
        e.message
    );

    node.kill_now();
}

#[tokio::test]
async fn order_by_and_having_resolve_select_aliases() {
    let (mut node, mut c) = qsem_node("alias").await;
    ddl(
        &mut c,
        "CREATE TABLE sal (id BIGINT PRIMARY KEY, base BIGINT NOT NULL, bonus BIGINT NOT NULL)",
    )
    .await;
    c.query_drop(
        "INSERT INTO sal (id, base, bonus) VALUES \
         (2, 5, 5), (4, 5, 0), (1, 20, 0), (3, 20, 20), (5, 45, 0)",
    )
    .await
    .expect("seed sal");

    // ORDER BY <alias> substitutes the projection: sums 5(id4), 10(id2),
    // 20(id1), 40(id3), 45(id5) -- no column is sorted that way.
    let got = col(&mut c, "SELECT id, base + bonus AS s FROM sal ORDER BY s").await;
    assert_eq!(got, vec![4, 2, 1, 3, 5]);
    // alias lookup is case-insensitive; DESC rides the substituted expr
    let got = col(
        &mut c,
        "SELECT id, base + bonus AS s FROM sal ORDER BY S DESC",
    )
    .await;
    assert_eq!(got, vec![5, 3, 1, 2, 4]);

    // HAVING over an aggregate alias (case-insensitive both ways)
    for sql in [
        "SELECT base, COUNT(*) AS cnt FROM sal GROUP BY base HAVING cnt > 1",
        "SELECT base, COUNT(*) AS cnt FROM sal GROUP BY base HAVING CNT > 1",
    ] {
        let mut got = rows(&mut c, sql).await;
        got.sort_by_key(|r| int_key(r));
        assert_eq!(
            got,
            vec![vec![int(5), int(2)], vec![int(20), int(2)]],
            "{sql}"
        );
    }

    // alias wins over a same-named FROM column: ORDER BY base is the
    // alias id*10 (10..50), not the source column (5,5,20,20,45).
    let got = col(
        &mut c,
        "SELECT id * 10 AS base, base FROM sal ORDER BY base",
    )
    .await;
    assert_eq!(got, vec![10, 20, 30, 40, 50], "select-list alias wins");

    // unknown ORDER BY name still fails validation with ER 1054
    let e = server_error(&mut c, "SELECT id FROM sal ORDER BY nosuch").await;
    assert_eq!(e.code, ER_BAD_FIELD_ERROR, "{}", e.message);
    assert!(
        e.message.to_lowercase().contains("unknown column"),
        "{}",
        e.message
    );

    node.kill_now();
}

#[tokio::test]
async fn limit_offset_placeholders_over_binary_protocol() {
    let (mut node, mut c) = qsem_node("limit").await;

    // text protocol with literals is unchanged
    let got = col(&mut c, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 2").await;
    assert_eq!(got, vec![3, 4]);

    // prepared (binary protocol): a bare LIMIT ?
    let stmt = c
        .prep("SELECT id FROM t ORDER BY id LIMIT ?")
        .await
        .expect("prep limit");
    let got: Vec<(i64,)> = c.exec(&stmt, (2i64,)).await.expect("exec LIMIT ?");
    assert_eq!(got, vec![(1,), (2,)]);

    // LIMIT ? OFFSET ? binds limit-then-offset
    let stmt = c
        .prep("SELECT id FROM t ORDER BY id LIMIT ? OFFSET ?")
        .await
        .expect("prep limit offset");
    let got: Vec<(i64,)> = c.exec(&stmt, (2i64, 1i64)).await.expect("exec");
    assert_eq!(got, vec![(2,), (3,)]);
    // taking past the end yields the tail
    let got: Vec<(i64,)> = c.exec(&stmt, (10i64, 3i64)).await.expect("tail");
    assert_eq!(got, vec![(4,)]);
    // offset alone can be the placeholder
    let stmt = c
        .prep("SELECT id FROM t ORDER BY id LIMIT 2 OFFSET ?")
        .await
        .expect("prep offset only");
    let got: Vec<(i64,)> = c.exec(&stmt, (2i64,)).await.expect("exec OFFSET ?");
    assert_eq!(got, vec![(3,), (4,)]);

    // post-bind values must be non-negative integers: negative,
    // fractional and string bindings all reject with the LIMIT error
    let stmt = c
        .prep("SELECT id FROM t ORDER BY id LIMIT ? OFFSET ?")
        .await
        .expect("prep bad");
    for (label, exec) in [
        (
            "negative",
            c.exec::<mysql_async::Row, _, _>(&stmt, (-1i64, 0i64)).await,
        ),
        (
            "fractional",
            c.exec::<mysql_async::Row, _, _>(&stmt, (2.5f64, 0i64))
                .await,
        ),
        (
            "string",
            c.exec::<mysql_async::Row, _, _>(&stmt, ("2", 0i64)).await,
        ),
    ] {
        let Err(mysql_async::Error::Server(e)) = exec else {
            panic!("{label} LIMIT binding must fail, got rows");
        };
        assert_eq!(e.code, ER_PARSE_ERROR, "{label}: {}", e.message);
        assert!(
            e.message.contains("LIMIT must be a non-negative integer"),
            "{label}: {}",
            e.message
        );
    }

    node.kill_now();
}

#[tokio::test]
async fn from_dual_behaves_like_no_table() {
    let (mut node, mut c) = qsem_node("dual").await;

    // unqualified, unaliased, case-insensitive: one synthetic row
    let got = rows(&mut c, "SELECT 1 FROM DUAL").await;
    assert_eq!(got, vec![vec![int(1)]]);
    let got = rows(&mut c, "SELECT 2 FROM dual").await;
    assert_eq!(got, vec![vec![int(2)]]);
    // scalar functions ride the no-FROM path too
    let got = rows(&mut c, "SELECT VERSION() FROM DuAl").await;
    assert_eq!(got.len(), 1, "VERSION() FROM dual yields one row");
    assert!(
        matches!(&got[0][0], MVal::Bytes(b) if !b.is_empty()),
        "version cell: {:?}",
        got[0][0]
    );

    // WHERE and LIMIT apply to the synthetic row
    let got = rows(&mut c, "SELECT 3 FROM DUAL WHERE 1 = 0").await;
    assert!(got.is_empty());

    // prepared: a bound LIMIT over DUAL (binary protocol)
    let stmt = c
        .prep("SELECT 1 FROM DUAL WHERE 1 = 1 LIMIT ?")
        .await
        .expect("prep dual");
    let got: Vec<(i64,)> = c.exec(&stmt, (5i64,)).await.expect("exec dual LIMIT ?");
    assert_eq!(got, vec![(1,)]);

    // a dual lookalike is still an ordinary (missing) table
    let e = server_error(&mut c, "SELECT 1 FROM dual2").await;
    assert_eq!(e.code, ER_NO_SUCH_TABLE, "{}", e.message);

    node.kill_now();
}

#[tokio::test]
async fn ordinal_negative_matrix() {
    let (mut node, mut c) = qsem_node("negative").await;

    // out-of-range ORDER BY ordinal: MySQL's ER 1054 wording
    let e = server_error(&mut c, "SELECT id FROM t ORDER BY 5").await;
    assert_eq!(e.code, ER_BAD_FIELD_ERROR, "{}", e.message);
    assert!(
        e.message.contains("Unknown column '5' in 'order clause'"),
        "{}",
        e.message
    );
    // 0 is not a 1-based position either
    let e = server_error(&mut c, "SELECT id FROM t ORDER BY 0").await;
    assert_eq!(e.code, ER_BAD_FIELD_ERROR, "{}", e.message);

    // an ordinal into a * projection cannot know its width: loud
    // not-supported, never a silent sort drop
    let e = server_error(&mut c, "SELECT * FROM t ORDER BY 1").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);
    assert!(
        e.message.to_lowercase().contains("projection"),
        "{}",
        e.message
    );

    // an ordinal pointing at a ? projection would duplicate the
    // parameter and desync binding counts: loud, already at prepare
    let e = prepare_error(&mut c, "SELECT id, ? AS p FROM t ORDER BY 2").await;
    assert_eq!(e.code, ER_NOT_SUPPORTED_YET, "{}", e.message);

    node.kill_now();
}

#[tokio::test]
async fn agg_wrappers_and_session_funcs_in_dml() {
    let (mut node, mut c) = qsem_node("aggwrap").await;
    ddl(
        &mut c,
        "CREATE TABLE d (id BIGINT PRIMARY KEY, v DECIMAL(10, 3) NOT NULL)",
    )
    .await;
    c.query_drop("INSERT INTO d (id, v) VALUES (1, 1.005), (2, 5.351)")
        .await
        .expect("seed d");

    // Scalar wrappers around aggregates: the Agg node must be
    // substituted before Func evaluation (previously ER 1235).
    let got = rows(&mut c, "SELECT ROUND(SUM(v), 2) FROM d").await;
    assert_eq!(got, vec![vec![MVal::Bytes(b"6.36".to_vec())]]);
    // An empty group's SUM is NULL: COALESCE supplies the fallback.
    let got = rows(&mut c, "SELECT COALESCE(SUM(v), 0) FROM d WHERE id > 10").await;
    assert_eq!(got, vec![vec![int(0)]]);

    // Session functions bind inside ODKU assignments: the conflicting
    // row's v becomes the CURRENT database (the connection has no
    // default db, so the engine default answers -- same as
    // SELECT DATABASE() on this connection).
    ddl(
        &mut c,
        "CREATE TABLE o (id BIGINT PRIMARY KEY, v VARCHAR(64) NOT NULL)",
    )
    .await;
    c.query_drop("INSERT INTO o (id, v) VALUES (1, 'first')")
        .await
        .expect("seed o");
    c.query_drop(
        "INSERT INTO o (id, v) VALUES (1, 'again') ON DUPLICATE KEY UPDATE v = DATABASE()",
    )
    .await
    .expect("odku with DATABASE()");
    let want = rows(&mut c, "SELECT DATABASE()").await;
    let got = rows(&mut c, "SELECT v FROM o").await;
    assert_eq!(got, want, "ODKU assignment got the connected db name");
    assert!(
        matches!(&got[0][0], MVal::Bytes(b) if !b.is_empty()),
        "db name cell: {:?}",
        got[0][0]
    );

    // UPDATE ORDER BY evaluates in-statement too: a session function
    // there sorts by a constant key (previously an unknown-function
    // rejection from the session-free evaluator).
    c.query_drop("UPDATE o SET v = USER() ORDER BY CONNECTION_ID() LIMIT 1")
        .await
        .expect("update order by session func");
    let got = rows(&mut c, "SELECT v FROM o").await;
    let want = rows(&mut c, "SELECT USER()").await;
    assert_eq!(got, want, "UPDATE ran and stored the session user");

    node.kill_now();
}
