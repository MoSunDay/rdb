//! Multi-store scenario #130 -- `migrate task` + `migrate list` on a REAL
//! two-node cluster (node0 bootstraps, node1 joins over raft-http,
//! `cluster init` carves the equal slot bands): 100 keys in ONE
//! hash-tag slot are seeded on their owner (slot from CLUSTER KEYSLOT,
//! owner from probe reads past -MOVED). Before the switch the
//! destination's IMPORTING gate is opened by hand and a COPY-landed
//! probe proves the one-shot ASKING rule (a plain read there is -MOVED
//! back to the source, ASKING reads the value), then the one-command
//! reshard runs: `migrate task <slot> <src> <dst>` -> +OK, `migrate
//! list` -> the JSON task record ("status":"done", "moved":100),
//! ownership switched (dst serves locally, src -> -MOVED to dst), and a
//! SQL audit row on the leader's MySQL plane mirrors the record.

use crate::common;
use crate::scenario_common;
use common::mysql::{connect_root, ddl, int, ints, pairs, rows as sql_rows, run, wait_table};
use common::{TOKEN, cluster_init, wait_cluster_nodes_list_all, wait_leader};
use scenario_common::{
    FrontSpec, front_world, json_of, ok, r, rb, rt, spawn_front_node, wait_fronts, world_dir,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const KEYS: usize = 100;
const TAG: &str = "mig130";

/// Key `i` of the one-slot group (`{mig130}k000`...).
fn key(i: usize) -> Vec<u8> {
    format!("{{{TAG}}}k{i:03}").into_bytes()
}

fn value(i: usize) -> String {
    format!("val-{i:03}")
}

/// Bulk payloads of a full-drain array reply (`$N` heads).
fn bulk_count(full: &str) -> usize {
    full.split("\r\n")
        .filter(|t| t.len() > 1 && t.starts_with('$') && t[1..].parse::<usize>().is_ok())
        .count()
}

/// AUTH + ASKING on a fresh connection, then `args` sent once per entry
/// of `reply_lines` on that ONE connection; entry `i` is how many CRLF
/// lines reply `i` spans (a bulk `$7\r\nval-000\r\n` is 2, a plain
/// `-MOVED ...` error is 1 -- the server writes the error and keeps the
/// connection open, `src/command/mod.rs`). Reads until `2 + sum` lines
/// arrived -- falling short inside 5s is a failure, not a silent wait --
/// and returns everything the server said after the two +OKs, so a
/// repeat shows the one-shot ASK flag expiring.
async fn asking_cmd(addr: &str, args: &[&[u8]], reply_lines: &[usize]) -> Vec<u8> {
    let want = 2 + reply_lines.iter().sum::<usize>();
    let mut sock = TcpStream::connect(addr).await.expect("connect");
    let mut buf = format!("*2\r\n$4\r\nAUTH\r\n${}\r\n{}\r\n", TOKEN.len(), TOKEN).into_bytes();
    buf.extend_from_slice(b"*1\r\n$6\r\nASKING\r\n");
    for _ in 0..reply_lines.len() {
        buf.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
        for a in args {
            buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            buf.extend_from_slice(a);
            buf.extend_from_slice(b"\r\n");
        }
    }
    sock.write_all(&buf).await.expect("write");
    let mut out = Vec::new();
    let mut lines = 0usize;
    let timed_out = tokio::time::timeout(Duration::from_secs(5), async {
        let mut chunk = [0u8; 1024];
        loop {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    out.extend_from_slice(&chunk[..n]);
                    lines = out.iter().filter(|b| **b == b'\n').count();
                    if lines >= want {
                        break;
                    }
                }
            }
        }
    })
    .await
    .is_err();
    assert!(
        !timed_out && lines >= want,
        "ASKING {args:?}: {lines}/{want} reply lines: {out:?}"
    );
    out.strip_prefix(b"+OK\r\n+OK\r\n")
        .map(<[u8]>::to_vec)
        .unwrap_or(out)
}

/// Every node's resp bind registered in the raft `sql_nodes` map with a
/// non-empty sql_rpc port (the registry self-registers on a 3s ticker).
async fn wait_sql_nodes(leader: &str, binds: &[String], secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let reg = String::from_utf8_lossy(&r(leader, &[b"raft", b"get", b"sql_nodes"]).await)
            .into_owned();
        let ready =
            binds.iter().all(|b| reg.contains(b.as_str())) && !reg.contains("\"sql_rpc\":\"\"");
        if ready {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "sql_nodes never converged: {reg}"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn migrate_task_moves_a_slot_and_lists_the_record() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        mysql: true,
        sql_rpc: true,
        ..Default::default()
    };
    let (n0, _) = front_world(TAG, &spec).await;
    let join = n0.node.http.clone();
    let mut n1 = spawn_front_node(&world_dir("mig130b"), 1, false, Some(&join), &spec);
    wait_fronts(&mut n1, true, false, false).await;
    let nodes = [n0.node, n1.node];
    let leader = wait_leader(&nodes, 30).await;
    let peers = [nodes[0].resp.clone(), nodes[1].resp.clone()];
    cluster_init(&nodes[leader], &peers).await;
    wait_cluster_nodes_list_all(&nodes, &peers, 90).await;
    let lead = nodes[leader].resp.clone();

    // a) The slot and its owner: CLUSTER KEYSLOT on the leader, the
    //    owner is the node whose read is NOT -MOVED.
    let slot: u16 = rt(&lead, &[b"CLUSTER", b"KEYSLOT", &key(0)])
        .await
        .strip_prefix(':')
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("keyslot reply"));
    let mut moved: Vec<String> = Vec::new();
    for p in &peers {
        moved.push(rt(p, &[b"GET", &key(0)]).await);
    }
    assert_eq!(
        moved.iter().filter(|m| m.starts_with("-MOVED")).count(),
        1,
        "exactly one owner before the migration: {moved:?}"
    );
    let src_i = moved.iter().position(|m| !m.starts_with("-MOVED")).unwrap();
    let (src, dst) = (peers[src_i].clone(), peers[1 - src_i].clone());
    assert!(
        moved[src_i].starts_with("$-1"),
        "owner holds nothing yet: {moved:?}"
    );

    // b) 100 keys seeded on the owner, all in the one slot.
    for i in 0..KEYS {
        let k = key(i);
        ok(&r(&src, &[b"SET", &k, value(i).as_bytes()]).await, "seed");
    }
    let in_slot = bulk_count(
        &scenario_common::rf(
            &src,
            &[
                b"CLUSTER",
                b"GETKEYSINSLOT",
                slot.to_string().as_bytes(),
                b"500",
            ],
        )
        .await,
    );
    assert_eq!(in_slot, KEYS, "all {KEYS} keys live in slot {slot}");
    assert_eq!(
        bulk_count(
            &scenario_common::rf(
                &dst,
                &[
                    b"CLUSTER",
                    b"GETKEYSINSLOT",
                    slot.to_string().as_bytes(),
                    b"500"
                ]
            )
            .await
        ),
        0,
        "destination holds none"
    );

    // c) ASKING one-shot BEFORE the switch: dst's IMPORTING gate opens
    //    for src, a COPY of key 0 lands there. A plain read on dst is
    //    -MOVED back to the source; the one-shot ASKING reads the value.
    let src_id = rdb::utils::md5_with40(&src);
    ok(
        &r(
            &dst,
            &[
                b"CLUSTER",
                b"SETSLOT",
                slot.to_string().as_bytes(),
                b"IMPORTING",
                src_id.as_bytes(),
            ],
        )
        .await,
        "setslot importing",
    );
    let (dh, dp) = dst.rsplit_once(':').expect("host:port");
    let k0 = key(0);
    ok(
        &r(
            &src,
            &[
                b"MIGRATE",
                dh.as_bytes(),
                dp.as_bytes(),
                &k0,
                b"0",
                b"5000",
                b"COPY",
            ],
        )
        .await,
        "migrate copy probe",
    );
    assert_eq!(
        rt(&dst, &[b"GET", &k0]).await,
        format!("-MOVED {slot} {src}"),
        "plain read on the importing node"
    );
    assert_eq!(
        asking_cmd(&dst, &[b"GET", &k0], &[2]).await,
        b"$7\r\nval-000\r\n",
        "ASKING reads the landed copy"
    );
    assert_eq!(
        asking_cmd(&dst, &[b"GET", &k0], &[2, 1]).await,
        format!("$7\r\nval-000\r\n-MOVED {slot} {src}\r\n").as_bytes(),
        "the ASK flag is single-shot: the repeat on the same connection is -MOVED"
    );
    // The landed copy must not block the task's own RESTORE: drop it
    // through the gate (ASKING DEL), the source keeps its COPY.
    assert_eq!(
        asking_cmd(&dst, &[b"DEL", &k0], &[1]).await,
        b":1\r\n",
        "ASKING DEL probe copy"
    );
    assert_eq!(
        rb(&src, &[b"GET", &k0]).await,
        "val-000",
        "COPY kept the source"
    );

    // d) The one-command reshard, driven on the leader.
    let t = scenario_common::perf::now();
    assert_eq!(
        rt(
            &lead,
            &[
                b"migrate",
                b"task",
                slot.to_string().as_bytes(),
                src.as_bytes(),
                dst.as_bytes()
            ]
        )
        .await,
        "+OK",
        "migrate task; {}",
        nodes[leader].ctx()
    );
    let record = rb(&lead, &[b"migrate", b"list"]).await;
    assert!(
        record.contains(r#""status":"done""#) && record.contains(r#""moved":100"#),
        "task record: {record}"
    );
    let doc = json_of(&record);
    assert_eq!(doc["slot"].as_u64(), Some(slot as u64), "{record}");
    assert_eq!(doc["src"].as_str(), Some(src.as_str()), "{record}");
    assert_eq!(doc["dst"].as_str(), Some(dst.as_str()), "{record}");
    assert_eq!(doc["status"].as_str(), Some("done"), "{record}");
    assert_eq!(doc["moved"].as_u64(), Some(KEYS as u64), "{record}");

    // e) Ownership switched: dst answers locally, src redirects to dst.
    assert_eq!(
        rb(&dst, &[b"GET", &k0]).await,
        "val-000",
        "dst serves key 0"
    );
    assert_eq!(
        rb(&dst, &[b"GET", &key(99)]).await,
        "val-099",
        "dst serves key 99"
    );
    assert_eq!(
        rt(&src, &[b"GET", &key(42)]).await,
        format!("-MOVED {slot} {dst}"),
        "src redirects"
    );
    assert_eq!(
        rb(&dst, &[b"GET", &key(42)]).await,
        "val-042",
        "the redirected key lives on dst"
    );
    assert_eq!(
        bulk_count(
            &scenario_common::rf(
                &dst,
                &[
                    b"CLUSTER",
                    b"GETKEYSINSLOT",
                    slot.to_string().as_bytes(),
                    b"500"
                ]
            )
            .await
        ),
        KEYS,
        "dst holds the slot"
    );
    assert_eq!(
        bulk_count(
            &scenario_common::rf(
                &src,
                &[
                    b"CLUSTER",
                    b"GETKEYSINSLOT",
                    slot.to_string().as_bytes(),
                    b"500"
                ]
            )
            .await
        ),
        0,
        "src drained"
    );
    scenario_common::perf::within("mig130/migrate-100-keys", t, Duration::from_secs(10));

    // f) SQL audit row on the leader's MySQL plane, mirroring the record.
    //    SQL tables are slot-banded over the cluster, so a SELECT scatters
    //    to every band owner over sql_rpc: both nodes must have registered
    //    their port in the raft-replicated `sql_nodes` registry (3s ticker)
    //    and the non-leader's catalog must carry the table first.
    wait_sql_nodes(&nodes[leader].resp, &peers, 30).await;
    let mut c = connect_root(&nodes[leader]).await;
    let mut follower = connect_root(&nodes[1 - leader]).await;
    ddl(
        &mut c,
        "CREATE TABLE mig_audit (slot INT PRIMARY KEY, src VARCHAR(48) NOT NULL, \
         dst VARCHAR(48) NOT NULL, moved INT NOT NULL, status VARCHAR(8) NOT NULL)",
    )
    .await;
    wait_table(&mut follower, "mig_audit", true).await;
    run(
        &mut c,
        &format!(
            "INSERT INTO mig_audit VALUES ({slot}, '{src}', '{dst}', {}, 'done')",
            doc["moved"].as_u64().unwrap_or(0)
        ),
    )
    .await;
    let audit = sql_rows(&mut c, "SELECT slot, moved FROM mig_audit").await;
    assert_eq!(
        audit,
        vec![vec![int(slot as i64), int(KEYS as i64)]],
        "one audit row: the slot, 100 moved"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM mig_audit WHERE status = 'done' AND moved > 0"
        )
        .await,
        vec![1]
    );
    assert_eq!(
        pairs(&mut c, "SELECT slot, CONCAT(src, '->', dst) FROM mig_audit").await,
        vec![(slot as i64, format!("{src}->{dst}"))],
        "audit endpoints match the task record"
    );
    scenario_common::perf::scenario("mig130", t0);
}
