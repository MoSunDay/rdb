//! Multi-store scenario #10 -- failover with warm data on THREE planes
//! (SQL catalog+rows, KV families, lite-MQ streams) across a REAL
//! 3-node raft cluster (start_sql_cluster bring-up: MySQL + 2PC rpc
//! fronts, CLUSTER INIT, converged sql_nodes registry). Kill -9 the
//! leader mid-flight: the survivor leader keeps accepting SQL writes
//! and KV/MQ writes for the slots it owns (data-plane routing follows
//! -MOVED to each key's owner), and the respawned old leader catches up
//! on everything committed while it was dead (process_failover /
//! sql_failover precedents, raft polling with generous deadlines).

mod common;
mod scenario_common;

use std::time::Duration;

use common::lite::text;
use mysql_async::prelude::Queryable;
use common::mysql::{connect_root, ints};
use common::{cmd_one_shot, start_sql_cluster, wait_leader, wait_resp_ready, ProcNode, TOKEN};

/// `routed` with the quiet-gap reader (array replies: XREADGROUP etc).
async fn routed_full(nodes: &[ProcNode], skip: Option<usize>, args: &[&[u8]]) -> (usize, Vec<u8>) {
    for (i, n) in nodes.iter().enumerate() {
        if Some(i) == skip {
            continue;
        }
        let reply = common::lite::cmd_full_reply(&n.resp, TOKEN, args, 250).await;
        if !text(&reply).starts_with("-MOVED") {
            return (i, reply);
        }
    }
    panic!("no alive owner answered {args:?}");
}

/// Run one RESP command against the first ALIVE node that owns it
/// (skipping `skip`, typically the corpse): returns (node idx, reply).
async fn routed(nodes: &[ProcNode], skip: Option<usize>, args: &[&[u8]]) -> (usize, Vec<u8>) {
    for (i, n) in nodes.iter().enumerate() {
        if Some(i) == skip {
            continue;
        }
        let reply = cmd_one_shot(&n.resp, TOKEN, args).await;
        if !text(&reply).starts_with("-MOVED") {
            return (i, reply);
        }
    }
    panic!("no alive owner answered {args:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill9_leader_multistore_carries_on_and_catches_up() {    // a) 3-node cluster: SQL (MySQL front + 2PC rpc) on every node.
    let dir = tempfile::tempdir().expect("tempdir");
    let mut nodes = start_sql_cluster(dir.path(), 3).await;
    let leader = wait_leader(&nodes, 60).await;

    // b) Warm the SQL plane on the leader: a table plus two rows.
    let mut c = connect_root(&nodes[leader]).await;
    common::mysql::ddl(
        &mut c,
        "CREATE TABLE warm (id BIGINT PRIMARY KEY, tag VARCHAR(32) NOT NULL)",
    )
    .await;
    common::mysql::run(&mut c, "INSERT INTO warm (id, tag) VALUES (1, 'pre-a'), (2, 'pre-b')").await;
    let pre = ints(&mut c, "SELECT COUNT(*) FROM warm").await;
    assert_eq!(pre, vec![2]);

    // c) Warm the KV plane with keys the LEADER owns: probe hash tags
    //    until one routes home to the leader (stream names forbid
    //    braces, but plain KV keys may carry them).
    let mut ha = String::new();
    for t in 0..48i32 {
        let tag = format!("{{p{t}}}");
        let (owner, rep) = routed(
            &nodes,
            None,
            &[b"HSET", format!("{tag}:sess").as_bytes(), b"u1", b"alive"],
        )
        .await;
        if owner == leader {
            assert!(text(&rep).starts_with(':'), "{rep:?}");
            ha = tag;
            break;
        }
    }
    assert!(!ha.is_empty(), "no probed tag landed on the leader");
    let (c_owner, cfg_reply) = routed(&nodes, None, &[b"SET", format!("{ha}:cfg").as_bytes(), b"v1"]).await;
    assert!(text(&cfg_reply).starts_with("+OK"), "{cfg_reply:?}");
    assert_eq!(c_owner, leader, "the {{ha}} tag pins these keys to the leader");

    // d) Warm the MQ plane: lite streams are raft-REPLICATED (XADD and
    //    group ops serve on any node), but `>` delivery runs on the
    //    leader -- so the consumer reads there.
    let pre_stream = "pre/q0";
    for i in 1..=3i64 {
        let rep = cmd_one_shot(
            &nodes[leader].resp,
            TOKEN,
            &[b"XADD", pre_stream.as_bytes(), format!("1-{i}").as_bytes(), b"evt", format!("e{i}").as_bytes()],
        )
        .await;
        assert!(text(&rep).contains(&format!("1-{i}")), "{rep:?}");
    }
    let g = cmd_one_shot(&nodes[leader].resp, TOKEN, &[b"XGROUP", b"CREATE", pre_stream.as_bytes(), b"ops", b"0-0"]).await;
    assert!(text(&g).starts_with("+OK"), "{g:?}");
    let drain = common::lite::cmd_full_reply(
        &nodes[leader].resp,
        TOKEN,
        &[b"XREADGROUP", b"GROUP", b"ops", b"c1", b"STREAMS", pre_stream.as_bytes(), b">"],
        250,
    )
    .await;
    assert!(text(&drain).contains("e3"), "drained all three: {drain:?}");

    // e) SIGKILL the leader; survivors elect.
    nodes[leader].kill_now();
    let new = wait_leader(&nodes, 120).await;
    assert_ne!(new, leader, "the corpse cannot still lead");

    // f) The SQL plane keeps accepting writes on the new leader for
    //    the bands survivors own; the corpse's band refuses LOUDLY
    //    (2pc participant unreachable -- never a partial commit).
    let mut nc = connect_root(&nodes[new]).await;
    let mut committed = 0usize;
    for id in 3..=14i64 {
        let sql = format!("INSERT INTO warm (id, tag) VALUES ({id}, 'post-{id}')");
        match nc.query_drop(sql.as_str()).await {
            Ok(()) => committed += 1,
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("2pc participant") || msg.contains("unreachable"),
                    "dead-band write must fail loudly, got: {msg}"
                );
            }
        }
    }
    assert!(committed >= 1, "no live-band write committed");
    let total: i64 = 2 + committed as i64;

    // g) The MQ plane continues on the new leader: a fresh stream,
    //    its group, and a `>` read driven to the watermark.
    let hb = "late/q0";
    let add1 = cmd_one_shot(&nodes[new].resp, TOKEN, &[b"XADD", hb.as_bytes(), b"1-1", b"evt", b"late1"]).await;
    let add2 = cmd_one_shot(&nodes[new].resp, TOKEN, &[b"XADD", hb.as_bytes(), b"1-2", b"evt", b"late2"]).await;
    assert!(text(&add1).contains("1-1") && text(&add2).contains("1-2"), "{add1:?} {add2:?}");
    let g2 = cmd_one_shot(&nodes[new].resp, TOKEN, &[b"XGROUP", b"CREATE", hb.as_bytes(), b"ops", b"0-0"]).await;
    assert!(text(&g2).starts_with("+OK"), "{g2:?}");
    let late = common::lite::cmd_full_reply(
        &nodes[new].resp,
        TOKEN,
        &[b"XREADGROUP", b"GROUP", b"ops", b"c2", b"STREAMS", hb.as_bytes(), b">"],
        250,
    )
    .await;
    assert!(text(&late).contains("late2"), "new stream flows: {late:?}");
    // The killed leader's slot keys stay MOVEd while it is dead: the
    // data plane honestly reports the outage instead of losing data.
    let outage = cmd_one_shot(&nodes[new].resp, TOKEN, &[b"GET", format!("{ha}:cfg").as_bytes()]).await;
    assert!(
        text(&outage).starts_with("-MOVED"),
        "the dead owner's slot must MOVED, got {outage:?}"
    );

    // h) Respawn the old leader: same config + data dir, no re-bootstrap.
    nodes[leader].respawn();
    wait_resp_ready(&mut nodes[leader], 60).await;
    // Catch-up: the rows committed while it was dead arrive by raft.
    let mut oc = connect_root(&nodes[leader]).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let caught = loop {
        let n = ints(&mut oc, "SELECT COUNT(*) FROM warm").await;
        if n == vec![total] {
            break n;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "old leader never caught up, last count {n:?}\n{}",
            nodes[leader].ctx()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(caught, vec![total], "pre-kill + post-kill rows all visible");
    // Its slots answer again: the pre-kill KV family and stream history
    // survived the crash and are back online.
    let (_, sess) = routed_full(&nodes, None, &[b"HGETALL", format!("{ha}:sess").as_bytes()]).await;
    assert!(text(&sess).contains("alive"), "{sess:?}");
    let (_, cfg) = routed(&nodes, None, &[b"GET", format!("{ha}:cfg").as_bytes()]).await;
    assert!(text(&cfg).contains("v1"), "{cfg:?}");
    let (_, len) = routed(&nodes, None, &[b"XLEN", pre_stream.as_bytes()]).await;
    assert_eq!(text(&len).trim(), ":3", "pre-kill stream intact");
    // The post-kill stream stays readable while the old leader is
    // down AND after it rejoins (survivors keep serving it; lite
    // reads do not fail over to the corpse's slots).
    let hist = common::lite::cmd_full_reply(
        &nodes[new].resp,
        TOKEN,
        &[b"XRANGE", hb.as_bytes(), b"-", b"+"],
        250,
    )
    .await;
    assert!(text(&hist).contains("late2"), "post-kill stream visible again: {hist:?}");
}
