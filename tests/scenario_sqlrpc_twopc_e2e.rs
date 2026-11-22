//! Multi-store scenario #90 -- a cross-store settlement over the
//! sql_rpc front (`FrontSpec{sql_rpc:true}` -- the only front face no
//! earlier scenario ever enabled): three REAL nodes each run RESP +
//! MySQL + sql_rpc, `cluster init` carves the slot bands and the
//! self-registration ticker fills the raft-replicated `sql_nodes`
//! registry so the MySQL front can reach every peer; one multi-row
//! INSERT of transfer legs spanning all three slot bands commits
//! through Prepare/Decide on every owner, each node's read gathers the
//! WHOLE ledger (nothing missed, nothing doubled), a duplicate leg_no
//! vetoes the REMOTE prepare (dup: rides out as a MySQL 1062-shaped
//! error, nothing lands anywhere), and /sql2pc/status answers unknown
//! for a never-seen txn id. One world, MySQL + sql_rpc fronts.

mod common;
mod scenario_common;

use std::time::Instant;

use common::{cluster_init, contains_bytes, wait_cluster_nodes_list_all, wait_leader, TOKEN};
use mysql_async::prelude::*;
use mysql_async::Value as MVal;
use scenario_common::{front_world, FrontSpec};

/// One settlement batch: 39 legs over 3 slot bands.
const LEGS: i64 = 39;

async fn connect_all(nodes: &[common::ProcNode]) -> Vec<mysql_async::Conn> {
    let mut out = Vec::new();
    for n in nodes {
        out.push(common::mysql::connect_root(n).await);
    }
    out
}

async fn leg_ids(c: &mut mysql_async::Conn) -> Vec<i64> {
    let rs: Vec<mysql_async::Row> = c.query("SELECT id FROM legs").await.expect("gather legs");
    let mut ids: Vec<i64> = rs
        .into_iter()
        .map(|r| match r.get::<MVal, _>(0) {
            Some(MVal::Bytes(b)) => String::from_utf8(b).unwrap().parse().unwrap(),
            v => panic!("non-bytes id cell {v:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn settlement_spans_shards_over_sql_rpc_and_vetoes_dupes() {
    let spec = FrontSpec {
        mysql: true,
        sql_rpc: true,
        ..Default::default()
    };
    // Three nodes: node0 bootstraps in its own root, joiners in a second
    // (same-tag spawn would wipe node0's dir).
    let (n0, _) = front_world("sq90a", &spec).await;
    let join = n0.node.http.clone();
    let dir = scenario_common::world_dir("sq90b");
    let mut n1 = scenario_common::spawn_front_node(&dir, 1, false, Some(&join), &spec);
    let mut n2 = scenario_common::spawn_front_node(&dir, 2, false, Some(&join), &spec);
    scenario_common::wait_fronts(&mut n1, true, false, false).await;
    scenario_common::wait_fronts(&mut n2, true, false, false).await;
    let nodes = [n0.node, n1.node, n2.node];
    let leader = wait_leader(&nodes, 60).await;
    let binds: Vec<String> = nodes.iter().map(|n| n.resp.clone()).collect();
    cluster_init(&nodes[leader], &binds).await;
    wait_cluster_nodes_list_all(&nodes, &binds, 90).await;

    // The sql_nodes registry converges (self-registration ticker: 3s).
    let deadline = Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let reg = common::cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"raft", b"get", b"sql_nodes"]).await;
        let ready = binds.iter().all(|b| contains_bytes(&reg, b.as_bytes()))
            && !contains_bytes(&reg, b"\"sql_rpc\":\"\"");
        if ready {
            break;
        }
        assert!(Instant::now() < deadline, "sql_nodes never converged: {reg:?}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    let mut cs = connect_all(&nodes).await;
    common::mysql::ddl(
        &mut cs[0],
        "CREATE TABLE legs (id BIGINT PRIMARY KEY, store VARCHAR(32) NOT NULL, \
         amount INT NOT NULL, leg_no VARCHAR(32) NOT NULL, UNIQUE KEY uk_leg (leg_no))",
    )
    .await;
    common::mysql::wait_table(&mut cs[0], "legs", true).await;

    // a) The settlement batch: 39 legs (A-store debits and B-store
    //    credits interleaved) spanning all three slot bands commits
    //    through 2PC over sql_rpc.
    let rows: Vec<String> = (0..LEGS)
        .map(|i| format!("({}, 'store{}', {}, 'L{:04}')", i + 1, i % 2, (i % 7) * 10 + 5, i + 1))
        .collect();
    common::mysql::run(
        &mut cs[0],
        &format!("INSERT INTO legs (id, store, amount, leg_no) VALUES {}", rows.join(", ")),
    )
    .await;
    let expect: Vec<i64> = (1..=LEGS).collect();
    for (i, c) in cs.iter_mut().enumerate() {
        assert_eq!(leg_ids(c).await, expect, "node {i} gathers the whole ledger");
    }

    // b) A duplicate leg_no vetoes the REMOTE prepare: the statement
    //    fails with the dup: reason (1062-shaped) and lands nowhere.
    let dup = "INSERT INTO legs (id, store, amount, leg_no) VALUES \
               (100, 'storeX', 1, 'L0007'), (101, 'storeY', 2, 'L9999')";
    let err = match cs[1].query_drop(dup).await {
        Ok(()) => panic!("dup leg insert must fail"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("dup:") || err.contains("Duplicate entry"),
        "remote veto reason rides out: {err}"
    );
    for (i, c) in cs.iter_mut().enumerate() {
        assert_eq!(leg_ids(c).await, expect, "node {i} unchanged after veto");
    }

    // c) /sql2pc/status: 401 without the token, unknown for a
    //    never-seen id.
    let raw = |path: &str| {
        let http = nodes[0].http.clone();
        let p = path.to_string();
        async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut s = tokio::net::TcpStream::connect(&http).await.unwrap();
            s.write_all(format!("GET {p} HTTP/1.1\r\nHost: rdb\r\nConnection: close\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        }
    };
    let unauth = raw("/sql2pc/status?id=ts1&node=x&raft-token=wrong").await;
    assert!(unauth.starts_with("HTTP/1.1 401"), "{unauth}");
    let unknown = raw(&format!("/sql2pc/status?id=ts424242&node=x&raft-token={TOKEN}")).await;
    assert!(unknown.starts_with("HTTP/1.1 200") && unknown.ends_with("unknown\n"), "{unknown}");
}
