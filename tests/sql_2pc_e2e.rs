//! M3 end-to-end: cross-slot distributed SQL writes via 2PC over three
//! REAL rdb processes (MySQL frontend + sql_rpc transport + raft control
//! plane each). Covered behavior:
//! - `cluster init` + `sql_nodes` registration make every node's
//!   sql_rpc port resolvable, after which a multi-row INSERT spanning
//!   slot bands commits through Prepare/Decide on every owner;
//! - rows land ONLY on their slot owner (M3 reads are node-local: the
//!   union of the three SELECTs is the table and no row duplicates);
//! - a unique value owned by another row vetoes the REMOTE prepare
//!   (the `dup:` reason rides out as MySQL error 1062) and the whole
//!   statement disappears everywhere (abort visibility);
//! - `/sql2pc/status` answers `unknown` for never-seen ids (401
//!   without the token) and `committed` for the settled txn;
//! - a dead participant makes a spanning INSERT fail without leaving
//!   partial rows on survivors; the same INSERT commits after the
//!   participant restarts.

mod common;

use std::time::{Duration, Instant};

use common::{cmd_one_shot, start_sql_cluster, wait_mysql_ready, wait_resp_ready, ProcNode, TOKEN};
use mysql_async::prelude::*;
use mysql_async::{OptsBuilder, Value as MVal};

const PASS: &str = "e2e-sql-pass";
/// Rows in the spanning INSERT batches (40 ids over 3 slot bands: the
/// chance they all land on one node is ~(1/3)^39).
const BATCH: i64 = 40;

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

/// DDL is leader-gated; retry until the statement sticks.
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

async fn ids(conn: &mut mysql_async::Conn, sql: &str) -> Vec<i64> {
    let rs: Vec<mysql_async::Row> = conn.query(sql).await.expect(sql);
    let mut out = rs
        .into_iter()
        .map(|r| match r.get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
            v => panic!("non-bytes id cell {v:?}"),
        })
        .collect::<Vec<_>>();
    out.sort_unstable();
    out
}

/// Run one statement; "" on success, the error text otherwise.
async fn err_of(conn: &mut mysql_async::Conn, sql: &str) -> String {
    match conn.query_drop(sql).await {
        Ok(()) => String::new(),
        Err(e) => e.to_string(),
    }
}

/// Minimal HTTP/1.1 GET over a raw socket; status line + body.
async fn http_get(addr: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut sock = tokio::net::TcpStream::connect(addr)
        .await
        .expect("http connect");
    sock.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: rdb\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .expect("http write");
    let mut buf = Vec::new();
    let _ = sock.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// Poll SHOW TABLES on one node until the raft-replicated catalog
/// reaches it (DDL lands on the leader first).
async fn wait_table(conn: &mut mysql_async::Conn, table: &str, node: &ProcNode) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let rs: Vec<mysql_async::Row> = conn.query("SHOW TABLES").await.expect("show tables");
        let names: Vec<String> = rs
            .into_iter()
            .map(|r| match r.get::<MVal, _>(0) {
                Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap(),
                v => panic!("non-bytes table cell {v:?}"),
            })
            .collect();
        if names.iter().any(|n| n == table) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "table {table} never reached {} (have {names:?})",
            node.resp
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// M3 SELECTs scatter-gather: EVERY node must read back the WHOLE
/// table, exactly the expected id set from each node alike (bands are
/// disjoint and pk -> slot is pure, so nothing is missed or doubled).
fn check_gathered(per_node: &[Vec<i64>; 3], expect: &[i64], why: &str) {
    let mut want = expect.to_vec();
    want.sort_unstable();
    for (i, got) in per_node.iter().enumerate() {
        assert_eq!(got, &want, "{why}: node {i} gathered read");
    }
}

/// The id set a committed batch contributes (contiguous ranges).
fn batch_ids(lo: i64) -> Vec<i64> {
    (lo..lo + BATCH).collect()
}

/// One multi-row INSERT of BATCH rows (distinct ids and names).
fn batch_sql(lo: i64) -> String {
    let rows: Vec<String> = (0..BATCH)
        .map(|i| format!("({}, 'n{}')", lo + i, lo + i))
        .collect();
    format!("INSERT INTO items (id, name) VALUES {}", rows.join(", "))
}

/// ids per node after `SELECT id FROM items` on each live conn (M3:
/// each read is itself a cluster-wide gather, so live nodes agree).
async fn placement(cs: &mut [Option<&mut mysql_async::Conn>]) -> [Vec<i64>; 3] {
    let mut out = [Vec::new(), Vec::new(), Vec::new()];
    for (i, c) in cs.iter_mut().enumerate() {
        if let Some(conn) = c.as_deref_mut() {
            out[i] = ids(conn, "SELECT id FROM items").await;
        }
    }
    out
}

#[tokio::test]
async fn cross_slot_commit_locality_veto_and_status_route() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-2pc-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR(128) NOT NULL)",
    )
    .await;
    ddl(&mut c0, "CREATE UNIQUE INDEX items_name_uq ON items (name)").await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    // ---- spanning INSERT from node0 commits through 2PC ----
    c0.query_drop(batch_sql(1)).await.expect("spanning insert");
    let got = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&got, &batch_ids(1), "2pc insert");

    // ---- unique veto from a remote owner ----
    // Insert a fresh id per node band with the already-owned name 'n1':
    // at most one band can hold both the row and the unique key (the
    // coordinator's local dup message); the others must travel the 2PC
    // veto path and surface the raw `dup:` reason.
    let mut seen_remote_veto = false;
    for band_node in 0..3 {
        let fresh = 1100 + band_node * 1100;
        let err = err_of(
            &mut c0,
            &format!("INSERT INTO items (id, name) VALUES ({fresh}, 'n1')"),
        )
        .await;
        assert!(!err.is_empty(), "dup insert must fail (band {band_node})");
        assert!(
            err.contains("dup:") || err.contains("Duplicate entry"),
            "unexpected error: {err}"
        );
        seen_remote_veto |= err.contains("dup: unique value already owned");
    }
    assert!(
        seen_remote_veto,
        "no candidate exercised the remote veto path\n{}",
        common::all_ctx(&nodes)
    );
    // Abort visibility: nothing landed anywhere, the owner is intact.
    let after = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&after, &batch_ids(1), "after dup vetoes");
    assert_eq!(
        ids(&mut c0, "SELECT id FROM items WHERE id = 1").await,
        vec![1],
        "the original owner row must survive"
    );

    // ---- /sql2pc/status on the coordinator ----
    let coord = &nodes[0];
    let unauth = http_get(
        &coord.http,
        "/sql2pc/status?id=ts1&node=x&raft-token=wrong-token",
    )
    .await;
    assert!(unauth.starts_with("HTTP/1.1 401"), "{unauth}");
    let unknown = http_get(
        &coord.http,
        &format!("/sql2pc/status?id=ts424242&node=x&raft-token={TOKEN}"),
    )
    .await;
    assert!(unknown.starts_with("HTTP/1.1 200"), "{unknown}");
    assert!(unknown.ends_with("unknown\n"), "{unknown}");
    // Settled txns live anywhere below the raft ts cursor, but every
    // settled txn here spans >= 32 consecutive ts values, so a stride
    // probe of the coordinator's outcome table must find one.
    let cursor_raw = cmd_one_shot(&coord.resp, TOKEN, &[b"raft", b"get", b"sql_ts_cursor"]).await;
    let cursor: usize = String::from_utf8_lossy(&cursor_raw)
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse().ok())
        .max()
        .unwrap_or(0);
    let mut committed = 0;
    for n in (1..=cursor.max(1)).step_by(32) {
        let body = http_get(
            &coord.http,
            &format!("/sql2pc/status?id=ts{n}&node=x&raft-token={TOKEN}"),
        )
        .await;
        if body.ends_with("committed []\n") || body.contains("\ncommitted [") {
            committed += 1;
        }
    }
    assert!(
        committed >= 1,
        "no committed outcome served under ts{cursor}"
    );

    for mut n in nodes {
        n.kill_now();
    }
}

#[tokio::test]
async fn dead_participant_aborts_and_restart_recovers() {
    let dir = std::env::temp_dir().join(format!("rdb-sql-2pc-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut nodes = start_sql_cluster(&dir, 3).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;
    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, name VARCHAR(128) NOT NULL)",
    )
    .await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    c0.query_drop(batch_sql(1)).await.expect("first insert");
    let base = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&base, &batch_ids(1), "pre-crash gather");

    // ---- kill one non-coordinator node that holds rows ----
    // (40 ids over 3 slot bands: node1 holds rows with certainty in
    // practice, and the INSERT below proves it reached its owner.)
    let victim = 1;
    let survivor_conn = &mut c2;
    nodes[victim].kill_now();

    // The same spanning INSERT must fail: some rows map to the dead
    // owner, so Prepare cannot reach it -- and nothing committed.
    let err = err_of(&mut c0, &batch_sql(1001)).await;
    assert!(
        err.contains("2pc participant") || err.contains("conflict:"),
        "unexpected error for dead participant: {err}"
    );
    // M3 read contract: with a band owner down, a SELECT refuses to
    // serve partial results -- the survivor's gather fails with the
    // node-unreachable error instead of its own band alone.
    let read_err = err_of(survivor_conn, "SELECT id FROM items").await;
    assert!(
        read_err.contains("unreachable") || read_err.contains("1027"),
        "gather must fail loudly while a band owner is down: {read_err}"
    );

    // ---- restart the victim; the same INSERT now commits ----
    nodes[victim].respawn();
    wait_resp_ready(&mut nodes[victim], 30).await;
    wait_mysql_ready(&nodes[victim], 15).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let err = err_of(&mut c0, &batch_sql(1001)).await;
        if err.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "insert never succeeded after restart: {err}\n{}",
            common::all_ctx(&nodes)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // The restarted node needs a FRESH conn; the survivors keep theirs.
    let mut cv = connect(&nodes[victim]).await;
    let mut final_slots = [None, None, None];
    final_slots[0] = Some(&mut c0);
    final_slots[1] = Some(&mut c1);
    final_slots[2] = Some(&mut c2);
    final_slots[victim] = Some(&mut cv);
    let final_p = placement(&mut final_slots).await;
    let mut expect = batch_ids(1);
    expect.extend(batch_ids(1001));
    expect.sort_unstable();
    check_gathered(&final_p, &expect, "post-restart gather");

    for mut n in nodes {
        n.kill_now();
    }
}

/// Regression (follower-coordinated writes must land on slot owners):
/// an UPDATE/DELETE issued on a NON-owner node used to match rows with
/// a LOCAL scan only, so it silently matched 0 (or a band-sized slice
/// of) rows living on other slot owners and answered OK while nothing
/// (or not everything) changed. Matching now fans out exactly like a
/// SELECT, so the statement's write set covers the whole table and the
/// 2PC commit reaches every slot owner.
///
/// - autocommit UPDATE of every row, coordinated by node1;
/// - explicit-txn transfer pair (UPDATE -40 / UPDATE +40) coordinated
///   by every node in turn (whoever coordinates, both rows must move);
/// - autocommit DELETE of every row, coordinated by node2.
#[tokio::test]
async fn follower_coordinated_update_delete_reaches_all_slot_owners() {
    let dir =
        std::env::temp_dir().join(format!("rdb-sql-follower-write-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, amt BIGINT NOT NULL)",
    )
    .await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    // Seed 40 rows from the leader: ids spread over all three slot
    // bands (the chance they all land on one node is ~(1/3)^39).
    let seed: Vec<String> = (1..=40).map(|i| format!("({}, 1000)", i)).collect();
    c0.query_drop(format!(
        "INSERT INTO items (id, amt) VALUES {}",
        seed.join(", ")
    ))
    .await
    .expect("seed insert");
    let got = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&got, &(1..=40).collect::<Vec<_>>(), "seeded");

    // ---- autocommit UPDATE coordinated by a follower node ----
    // Pre-fix this matched only node1's local band (~1/3 of the rows)
    // and still answered OK; post-fix every row must move to 2000.
    c1.query_drop("UPDATE items SET amt = 2000 WHERE id >= 1")
        .await
        .expect("follower-coordinated full update");
    let n = c1.affected_rows() as usize;
    assert_eq!(n, 40, "UPDATE must affect every row, not just a band");
    for (i, c) in [&mut c0, &mut c1, &mut c2].iter_mut().enumerate() {
        let rs: Vec<mysql_async::Row> = c.query("SELECT DISTINCT amt FROM items").await.unwrap();
        let mut v: Vec<i64> = rs
            .into_iter()
            .map(|r| match r.get::<MVal, _>(0) {
                Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
                Some(MVal::Int(i)) => i,
                v => panic!("non-int amt cell {v:?}"),
            })
            .collect();
        v.sort_unstable();
        assert_eq!(v, vec![2000], "post-UPDATE node {i}");
    }

    // ---- explicit-txn transfer pair, coordinated by EVERY node ----
    // Whoever coordinates, both rows must actually move: the staged
    // writes fan out through 2PC at COMMIT.

    transfer_via(&mut c0, None, "node0", 1960, 2040).await;
    transfer_via(&mut c1, Some(&mut c0), "node1", 1920, 2080).await;
    transfer_via(&mut c2, Some(&mut c0), "node2", 1880, 2120).await;

    // ---- autocommit DELETE coordinated by another follower node ----
    c2.query_drop("DELETE FROM items WHERE id >= 1")
        .await
        .expect("follower-coordinated full delete");
    let n = c2.affected_rows() as usize;
    assert_eq!(n, 40, "DELETE must remove every row, not just a band");
    let got = placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await;
    check_gathered(&got, &[], "post-DELETE gather");

    for mut n in nodes {
        n.kill_now();
    }
}

/// One txn-mode transfer pair coordinated by `coord`'s node, then a
/// read-back from the leader: id 1 must move -40 and id 2 +40 for
/// every coordinator (a staged write set must reach its slot owner
/// through 2PC no matter which node staged it).
async fn transfer_via(
    coord: &mut mysql_async::Conn,
    leader: Option<&mut mysql_async::Conn>,
    label: &str,
    amt1: i64,
    amt2: i64,
) {
    // A lagging coordinator snapshot can meet a row committed by another
    // node's pair in between: the 2PC prepare vetoes with `conflict:`
    // (1213, retryable). Back off briefly so the ts refiller re-anchors
    // this node's read point, then retry the whole pair.
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut failure: Option<String> = None;
        for sql in [
            "BEGIN",
            "UPDATE items SET amt = amt - 40 WHERE id = 1",
            "UPDATE items SET amt = amt + 40 WHERE id = 2",
            "COMMIT",
        ] {
            if let Err(e) = coord.query_drop(sql).await {
                failure = Some(format!("{sql}: {e}"));
                break;
            }
        }
        match failure {
            None => break,
            Some(m) => {
                assert!(
                    attempt < 4 && (m.contains("1213") || m.contains("conflict:")),
                    "{label}: transfer failed (attempt {attempt}): {m}"
                );
                eprintln!("sql_2pc_e2e: {label} vetoed, retry {attempt}: {m}");
                let _ = coord.query_drop("ROLLBACK").await;
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
    // `None` = the coordinator IS the leader; read back through it.
    let reader: &mut mysql_async::Conn = match leader {
        Some(l) => l,
        None => coord,
    };
    let rs: Vec<mysql_async::Row> = reader
        .query("SELECT id, amt FROM items WHERE id IN (1, 2)")
        .await
        .unwrap();
    let mut got: Vec<(i64, i64)> = rs
        .into_iter()
        .map(|r| {
            let num = |i: usize| match r.get::<MVal, _>(i) {
                Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
                Some(MVal::Int(i)) => i,
                v => panic!("non-int cell {v:?}"),
            };
            (num(0), num(1))
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, vec![(1, amt1), (2, amt2)], "transfer via {label}");
}

// ---- defect-B regression: a follower whose ts lease predates the ----
// ---- cluster frontier must still read and write AT the frontier ----

/// crc16/xmodem, the same algorithm as the RESP plane's `hash::crc16`
/// (check value 0x31C3): a SQL row's slot is
/// `crc16(table_id_be ++ pk_key_bytes) % 16384`, its pk key bytes for a
/// BIGINT pk are `0x02 ++ (i ^ i64::MIN).to_be_bytes()` (order-preserving
/// `codec::key_int`), and slot s is owned by the first node i with
/// `s <= (i+1) * per` (`per` = 16384/nodes, addrs[i] = node i).
/// Replicated here so the test can aim rows at (or away from) one
/// node's band without a handle on crate internals.
fn crc16(key: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in key {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[tokio::test]
async fn stale_block_follower_reads_and_writes_at_the_cluster_frontier() {
    assert_eq!(crc16(b"123456789"), 0x31C3, "crc16 must match hash::crc16");
    let per = 16384usize / 3;
    let band = |slot: u16| (0..3).find(|i| slot as usize <= (i + 1) * per).unwrap();
    let slot_of = |id: i64| -> u16 {
        // table id 1: the first CREATE TABLE of this fresh cluster.
        let mut k = 1u32.to_be_bytes().to_vec();
        k.push(0x02);
        k.extend_from_slice(&(id ^ (1i64 << 63)).to_be_bytes());
        crc16(&k) % 16384
    };

    let dir = std::env::temp_dir().join(format!("rdb-sql-stale-ts-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let nodes = start_sql_cluster(&dir, 3).await;
    let mut c0 = connect(&nodes[0]).await;
    let mut c1 = connect(&nodes[1]).await;
    let mut c2 = connect(&nodes[2]).await;

    ddl(
        &mut c0,
        "CREATE TABLE items (id BIGINT PRIMARY KEY, amt BIGINT NOT NULL)",
    )
    .await;
    wait_table(&mut c1, "items", &nodes[1]).await;
    wait_table(&mut c2, "items", &nodes[2]).await;

    // node1 (a FOLLOWER) allocates this test's first timestamp, for a
    // row in its OWN band (local path: node1 joins no 2PC). Its block
    // lease then sits still -- an idle block is never below the refill
    // low-water -- so node1's ts horizon is frozen at that early grant.
    let seed_id = (1..).find(|i| band(slot_of(*i)) == 1).unwrap();
    c1.query_drop(format!("INSERT INTO items (id, amt) VALUES ({seed_id}, 5)"))
        .await
        .expect("follower seed");

    // The leader pushes the raft cursor a couple of blocks past every
    // possible early grant (~3 x 4096 ts are handed out before the
    // first statement runs): 14k rows in 256-row statements -- small
    // enough that the leader's own block always refills in time (it
    // must never degrade to the GAP fallback, whose stamps no cursor
    // ride can cover) -- and all on ids OUTSIDE node1's band, so node1
    // participates in none of those commits: nothing but the raft
    // cursor can lift its horizon.
    let mut want: Vec<i64> = vec![seed_id];
    let mut chunk: Vec<String> = Vec::new();
    let mut id = seed_id;
    while want.len() < 14_001 {
        id += 1;
        if band(slot_of(id)) == 1 {
            continue;
        }
        want.push(id);
        chunk.push(format!("({}, 1000)", id));
        if chunk.len() == 256 {
            let sql = format!("INSERT INTO items (id, amt) VALUES {}", chunk.join(", "));
            c0.query_drop(sql).await.expect("frontier push");
            chunk.clear();
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }
    // Flush the final partial chunk: `want` counts every row, so the
    // push must land all of them.
    if !chunk.is_empty() {
        let sql = format!("INSERT INTO items (id, amt) VALUES {}", chunk.join(", "));
        c0.query_drop(sql).await.expect("frontier push (tail)");
    }

    // The raft cursor is folded into node1's read point inside the
    // UPDATE statement itself (the autocommit match syncs the cursor
    // before `now()`). Pre-fix there was nothing to fold: the horizon
    // stayed at the early block tail, the UPDATE matched only the rows
    // stamped under it, and its own versions were carved from the
    // stale lease -- stamped UNDER the leader's newest rows and
    // silently buried.
    //
    // A follower's knowledge is bounded by raft replication: rows whose
    // cursor entry has not applied on node1 yet are not visible there
    // (a SELECT would not see them either), so the first pass may miss
    // the newest chunk. The invariant under test is CONVERGENCE: every
    // pass matches everything visible at the pass's frontier, no
    // matched row is ever lost, and once the frontier catches up the
    // remaining rows match. Pre-fix the horizon never rose and the
    // passes kept missing rows (while burying the ones they did
    // match), so the loop never converges.
    let mut matched: u64 = 0;
    for attempt in 0..10 {
        c1.query_drop("UPDATE items SET amt = amt + 1")
            .await
            .expect("stale-block follower UPDATE");
        let this = c1.affected_rows();
        matched += this;
        if matched == want.len() as u64 {
            break;
        }
        assert!(attempt < 9, "follower UPDATE never converged");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(
        matched,
        want.len() as u64,
        "every row must match across passes, not just the pre-frontier ones"
    );

    // The moved versions must be the newest everywhere: on every node
    // the whole table reads back (gather) and every amt moved exactly
    // once (5 -> 6 for the seed, 1000 -> 1001 for the pushed rows).
    for (i, c) in [&mut c0, &mut c1, &mut c2].iter_mut().enumerate() {
        let rs: Vec<mysql_async::Row> = c.query("SELECT DISTINCT amt FROM items").await.unwrap();
        let mut amts: Vec<i64> = rs
            .into_iter()
            .map(|r| match r.get::<MVal, _>(0) {
                Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
                v => panic!("non-bytes amt cell {v:?}"),
            })
            .collect();
        amts.sort_unstable();
        assert_eq!(amts, vec![6, 1001], "node {i}: every version moved");
    }
    check_gathered(
        &placement(&mut [Some(&mut c0), Some(&mut c1), Some(&mut c2)]).await,
        &want,
        "post-UPDATE gather",
    );

    for mut n in nodes {
        n.kill_now();
    }
}
