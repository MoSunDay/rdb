//! P3 coordinator failure paths against the REAL binary:
//! (a) session-timeout expiry -- a member that stops heartbeating is
//!     swept out, the group goes Empty, the zombie's Heartbeat answers
//!     UNKNOWN_MEMBER_ID and a fresh consumer rejoins natively;
//! (b) restart durability -- membership is coordinator-local (a
//!     respawned process reports the group "Dead"), while committed
//!     offsets stay in the durable ledger and a rejoin rebuilds the
//!     group at a fresh generation that still fences older ones.

mod common;
mod kafka_front_common;

use kafka_front_common::groups::{
    commit_v2, describe_v0, fetch_offset_v0, heartbeat_v0, join_v1, leave_v0, sync_v1,
};
use kafka_front_common::{resp_one_shot, spawn_kafka_node, wait_accepting};
use rdb::kafka::errors;
use tokio::net::TcpStream;

/// (a) Session-timeout sweep: session 1500ms, sweep cadence 500ms.
#[tokio::test]
async fn session_timeout_sweeps_silent_member_to_rebalance() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    resp_one_shot(&resp, &[b"XADD", b"t/q0", b"*", b"k1", b"v1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // A short-session member joins, syncs and goes silent.
    // (Empty member id -> MEMBER_ID_REQUIRED per KIP-394.)
    let r = join_v1(&mut sock, 1, "g1", 1500, 3000, "", b"sub[t0]").await;
    assert_eq!(r.error, errors::MEMBER_ID_REQUIRED);
    let id = r.member_id.clone();
    let r = join_v1(&mut sock, 2, "g1", 1500, 3000, &id, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 1);
    let (err, _) = sync_v1(&mut sock, 3, "g1", &id, 1, &[(id.as_str(), b"[t0]")]).await;
    assert_eq!(err, errors::NONE);
    assert_eq!(describe_v0(&mut sock, 4, "g1").await.state, "Stable");

    // No heartbeats: within ~ session + sweep the member is gone and
    // the group is Empty (generation bumped for zombie fencing).
    let mut swept = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if describe_v0(&mut sock, 100, "g1").await.state == "Empty" {
            swept = true;
            break;
        }
    }
    assert!(swept, "the sweeper emptied the silent group");

    // The zombie's heartbeat: it is no longer a member.
    assert_eq!(heartbeat_v0(&mut sock, 5, "g1", 1, &id).await, errors::UNKNOWN_MEMBER_ID);
    // Its sync is fenced as well.
    let (err, _) = sync_v1(&mut sock, 6, "g1", &id, 1, &[]).await;
    assert_eq!(err, errors::UNKNOWN_MEMBER_ID);

    // A fresh consumer rejoins natively (new id, fresh generation) and
    // may commit at the new generation.
    let r = join_v1(&mut sock, 7, "g1", 30_000, 60_000, "", b"sub[t0]").await;
    let id2 = r.member_id.clone();
    let r = join_v1(&mut sock, 8, "g1", 30_000, 60_000, &id2, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    assert!(r.generation >= 2, "the Empty transition bumped the generation");
    let gen2 = r.generation;
    let (err, _) = sync_v1(&mut sock, 9, "g1", &id2, gen2, &[(id2.as_str(), b"[t0]")]).await;
    assert_eq!(err, errors::NONE);
    assert_eq!(describe_v0(&mut sock, 10, "g1").await.state, "Stable");
    assert_eq!(commit_v2(&mut sock, 11, "g1", gen2, &id2, "t", 0, 3).await, errors::NONE);
    // The swept member fails the MEMBERSHIP check first; the surviving
    // member at a stale generation fails the generation check.
    assert_eq!(commit_v2(&mut sock, 12, "g1", 1, &id, "t", 0, 9).await, errors::UNKNOWN_MEMBER_ID);
    assert_eq!(commit_v2(&mut sock, 13, "g1", gen2 - 1, &id2, "t", 0, 9).await, errors::ILLEGAL_GENERATION);
}

/// (b) Restart: membership is in-memory, committed offsets are not.
#[tokio::test]
async fn restart_wipes_membership_but_keeps_offsets() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    resp_one_shot(&resp, &[b"XADD", b"t/q0", b"*", b"k1", b"v1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("connect kafka");

    // Stable group pushed to generation >= 2 BEFORE the restart: the
    // member leaves (last leaver -> Empty, runtime gen bumped to 2)
    // and rejoins (barrier -> gen 3); committing at gen 3 stamps the
    // ledger row's generation to 3 -- the exact shape of the restart
    // death zone (pre-fix: a post-restart group rebuilt at runtime
    // gen 1 was fenced by the ledger's 3 on every commit).
    let r = join_v1(&mut sock, 1, "g1", 30_000, 60_000, "", b"sub[t0]").await;
    let id = r.member_id.clone();
    let r = join_v1(&mut sock, 2, "g1", 30_000, 60_000, &id, b"sub[t0]").await;
    assert_eq!(r.generation, 1);
    let (err, _) = sync_v1(&mut sock, 3, "g1", &id, 1, &[(id.as_str(), b"[t0]")]).await;
    assert_eq!(err, errors::NONE);
    assert_eq!(commit_v2(&mut sock, 4, "g1", 1, &id, "t", 0, 7).await, errors::NONE);
    // Leave -> Empty (gen 2), rejoin -> barrier (gen 3), commit at 3.
    assert_eq!(leave_v0(&mut sock, 5, "g1", &id).await, errors::NONE);
    let r = join_v1(&mut sock, 6, "g1", 30_000, 60_000, &id, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 3, "Empty bumped to 2, the rejoin barrier to 3");
    let (err, _) = sync_v1(&mut sock, 7, "g1", &id, 3, &[(id.as_str(), b"[t0]")]).await;
    assert_eq!(err, errors::NONE);
    assert_eq!(commit_v2(&mut sock, 8, "g1", 3, &id, "t", 0, 7).await, errors::NONE);
    assert_eq!(fetch_offset_v0(&mut sock, 9, "g1", "t", 0).await, 7);

    // Kill + respawn the SAME data dir: the group is "Dead" (in-memory
    // membership gone), the ledger row (generation 3) survives.
    node.kill_now();
    node.respawn();
    wait_accepting(&kafka, &mut node, "kafka").await;
    let mut sock = TcpStream::connect(&kafka).await.expect("reconnect kafka");
    let d = describe_v0(&mut sock, 10, "g1").await;
    assert_eq!(d.state, "Dead");
    assert_eq!(d.member_ids.len(), 0);
    assert_eq!(fetch_offset_v0(&mut sock, 11, "g1", "t", 0).await, 7, "offsets are durable");

    // The pre-restart member is fenced against the LEDGER generation
    // (degraded layer 2: the group is absent from the runtime).
    assert_eq!(commit_v2(&mut sock, 12, "g1", 2, &id, "t", 0, 9).await, errors::ILLEGAL_GENERATION);

    // The client's native recovery: the FIRST join of the new process
    // seeds the runtime generation from the ledger (high-water 3), so
    // the rebuilt group starts ABOVE every pre-restart row -- no
    // death zone, commits succeed at the fresh generation right away.
    let r = join_v1(&mut sock, 13, "g1", 30_000, 60_000, "", b"sub[t0]").await;
    let id2 = r.member_id.clone();
    let r = join_v1(&mut sock, 14, "g1", 30_000, 60_000, &id2, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 4, "runtime resumes above the ledger gen 3");
    let (err, _) = sync_v1(&mut sock, 15, "g1", &id2, 4, &[(id2.as_str(), b"[t0]")]).await;
    assert_eq!(err, errors::NONE);
    assert_eq!(describe_v0(&mut sock, 16, "g1").await.state, "Stable");
    // Regression: pre-fix this was 22 ILLEGAL_GENERATION (ledger 3 vs
    // runtime 1); now the fresh member commits unimpeded.
    assert_eq!(commit_v2(&mut sock, 17, "g1", 4, &id2, "t", 0, 8).await, errors::NONE);
    // The old member id no longer exists: fenced by the runtime layer.
    assert_eq!(commit_v2(&mut sock, 18, "g1", 4, &id, "t", 0, 9).await, errors::UNKNOWN_MEMBER_ID);
    assert_eq!(fetch_offset_v0(&mut sock, 19, "g1", "t", 0).await, 8);
}
