//! Batch 11 scenario #114 -- consumer fleet churn: the two-phase
//! JoinGroup/SyncGroup handshake lands a single-member `fleet114` group
//! (79 on the empty member id, generation 1 on the rejoin), Heartbeat and
//! OffsetCommit v2 / OffsetFetch v0 keep its offset, ListGroups v1 reports
//! it Stable, and the RESP plane REFUSES to trim the stream the group
//! committed to (`XTRIM` trips the committed-offset ledger guard) until
//! LeaveGroup + DeleteGroups tear the group down. Ten more groups then
//! churn through the same join/sync/heartbeat/leave/delete cycle, and the
//! fleet ledger is a SQL table. One REAL node: Kafka + MySQL + RESP.

use std::time::Duration;

use crate::common::mysql::{connect_root, ddl, ints, run};
use crate::kafka_front_common;
use crate::scenario_common;
use crate::scenario_common::kafka::kafka_produce_v3;
use crate::scenario_common::{front_world, int_of, r, rt, FrontSpec};
use kafka_front_common::groups::{
    commit_v2, delete_groups, fetch_offset_v0, heartbeat_v0, join_v1, leave_v0, list_groups,
    sync_v1,
};
use tokio::net::TcpStream;

const GROUP: &str = "fleet114";
const STREAM: &str = "fleet114/q0";
const SESSION_MS: i32 = 30_000;
const REBALANCE_MS: i32 = 5_000;
const ROUNDS: usize = 10;

/// One churn round on `group`: join (79 + rejoin), sync, heartbeat,
/// leave, delete. Returns the next correlation id and the DeleteGroups
/// result (asserted by the caller, one row per group).
async fn churn_round(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
) -> (i32, i32, Vec<(String, i16)>) {
    let first = join_v1(
        sock,
        corr,
        group,
        SESSION_MS,
        REBALANCE_MS,
        "",
        b"sub[fleet]",
    )
    .await;
    assert_eq!(first.error, 79, "{group}: member id required");
    // KIP-394 reply echoes the group's CURRENT generation: 0 for a
    // group that has not barriered yet (the broker keeps no -1 state).
    assert_eq!(first.generation, 0, "{group}: no barrier yet");
    let member = first.member_id.clone();
    assert!(!member.is_empty(), "{group}: broker assigned an id");
    let joined = join_v1(
        sock,
        corr + 1,
        group,
        SESSION_MS,
        REBALANCE_MS,
        &member,
        b"sub[fleet]",
    )
    .await;
    assert_eq!(joined.error, 0, "{group}: rejoin clean");
    assert_eq!(joined.generation, 1, "{group}: solo barrier generation");
    assert_eq!(joined.leader, member, "{group}: joiner leads");
    assert_eq!(joined.members.len(), 1, "{group}: one member row");
    let (err, assignment) = sync_v1(
        sock,
        corr + 2,
        group,
        &member,
        joined.generation,
        &[(member.as_str(), b"[p0]")],
    )
    .await;
    assert_eq!(err, 0, "{group}: sync clean");
    assert_eq!(assignment, b"[p0]", "{group}: assignment echoes");
    assert_eq!(
        heartbeat_v0(sock, corr + 3, group, joined.generation, &member).await,
        0,
        "{group}: heartbeat clean"
    );
    assert_eq!(
        leave_v0(sock, corr + 4, group, &member).await,
        0,
        "{group}: leave clean"
    );
    let deleted = delete_groups(sock, corr + 5, 1, &[group]).await;
    (corr + 6, joined.generation, deleted)
}

#[tokio::test]
async fn fleet_churn_join_commit_ledger_guard_and_ten_rounds() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec {
        kafka: true,
        mysql: true,
        ..Default::default()
    };
    let (front, resp) = front_world("fleet114", &spec).await;

    // a) Seed the topic the fleet reads: the implicit q0 queue plus two
    //    Produce v3 records on top of it.
    let seed = r(&resp, &[b"XADD", b"fleet114", b"*", b"k", b"seed"]).await;
    assert!(seed.starts_with(b"*2"), "xadd seed: {seed:?}");
    for _ in 0..2 {
        let record = rdb::kafka::record::BatchRecord {
            timestamp_delta: 0,
            key: Some(b"fleet"),
            value: Some(b"telemetry"),
            headers: vec![],
        };
        kafka_produce_v3(&front.kafka, 1, "fleet114", &[record]).await;
    }
    assert_eq!(
        int_of(&r(&resp, &[b"XLEN", STREAM.as_bytes()]).await),
        3,
        "q0 holds the seed plus two produced records"
    );

    let mut sock = TcpStream::connect(&front.kafka)
        .await
        .expect("connect kafka front");
    let mut corr = 10;

    // b) The handshake: empty member -> 79 + the broker-assigned id.
    let first = join_v1(&mut sock, corr, GROUP, SESSION_MS, REBALANCE_MS, "", b"sub").await;
    corr += 1;
    assert_eq!(first.error, 79, "member id required");
    // The 79 reply carries the group's current generation, which is 0
    // until the first barrier (real Kafka/RDB behavior, not -1).
    assert_eq!(first.generation, 0, "no barrier before the rejoin");
    let member = first.member_id.clone();
    assert!(member.starts_with("rdb-fleet114-"), "assigned id: {member}");
    let joined = join_v1(
        &mut sock,
        corr,
        GROUP,
        SESSION_MS,
        REBALANCE_MS,
        &member,
        b"sub",
    )
    .await;
    corr += 1;
    assert_eq!(joined.error, 0, "rejoin clean");
    assert_eq!(joined.generation, 1, "solo barrier generation");
    assert_eq!(joined.members.len(), 1, "one member row");
    assert_eq!(joined.leader, member, "the joiner is the leader");

    // c) Steady state: sync, heartbeat, commit at offset 2, read back.
    let (err, assignment) = sync_v1(
        &mut sock,
        corr,
        GROUP,
        &member,
        1,
        &[(member.as_str(), b"[p0]")],
    )
    .await;
    corr += 1;
    assert_eq!((err, assignment), (0, b"[p0]".to_vec()), "sync clean");
    assert_eq!(
        heartbeat_v0(&mut sock, corr, GROUP, 1, &member).await,
        0,
        "heartbeat clean"
    );
    corr += 1;
    assert_eq!(
        commit_v2(&mut sock, corr, GROUP, 1, &member, "fleet114", 0, 2).await,
        0,
        "commit clean"
    );
    corr += 1;
    assert_eq!(
        fetch_offset_v0(&mut sock, corr, GROUP, "fleet114", 0).await,
        2,
        "committed offset echoes"
    );
    corr += 1;
    let listed = list_groups(&mut sock, corr, 1, &[]).await;
    corr += 1;
    assert!(
        listed.contains(&(
            GROUP.to_string(),
            "consumer".to_string(),
            "Stable".to_string()
        )),
        "fleet114 is a Stable consumer group: {listed:?}"
    );

    // d) The RESP ledger guard: a committed group pins the stream, so a
    //    trim is refused until the group is destroyed.
    let refused = rt(&resp, &[b"XTRIM", STREAM.as_bytes(), b"MAXLEN", b"0"]).await;
    assert!(
        refused.contains("committed consumer-group offsets"),
        "XTRIM must trip the ledger guard: {refused:?}"
    );
    assert!(
        refused.starts_with("-ERR"),
        "guard is an error reply: {refused:?}"
    );
    assert_eq!(
        int_of(&r(&resp, &[b"XLEN", STREAM.as_bytes()]).await),
        3,
        "the refused trim dropped nothing"
    );
    assert_eq!(
        leave_v0(&mut sock, corr, GROUP, &member).await,
        0,
        "leave clean"
    );
    corr += 1;
    assert_eq!(
        delete_groups(&mut sock, corr, 1, &[GROUP]).await,
        vec![(GROUP.to_string(), 0)],
        "group deleted"
    );
    // With the group gone the same trim is ALLOWED: `MAXLEN 0` drops
    // every entry, so the reply is the count it trimmed (3), not :0 --
    // the stream held seed + two produced records.
    assert_eq!(
        int_of(&r(&resp, &[b"XTRIM", STREAM.as_bytes(), b"MAXLEN", b"0"]).await),
        3,
        "with the group gone the trim is allowed and drops all three"
    );
    assert_eq!(
        int_of(&r(&resp, &[b"XLEN", STREAM.as_bytes()]).await),
        0,
        "the stream is empty after the allowed trim"
    );

    // e) Ten distinct groups churn through the same cycle.
    let mut ledger: Vec<(String, i32)> = Vec::with_capacity(ROUNDS);
    let t = scenario_common::perf::now();
    for i in 0..ROUNDS {
        let group = format!("churn{i}");
        let (next, generation, deleted) = churn_round(&mut sock, corr, &group).await;
        corr = next;
        assert_eq!(deleted, vec![(group.clone(), 0)], "{group}: deleted");
        ledger.push((group, generation));
    }
    scenario_common::perf::within("fleet114/10-rounds", t, Duration::from_secs(5));

    // f) The fleet ledger in SQL: one row per churned group.
    let mut c = connect_root(&front.node).await;
    ddl(
        &mut c,
        "CREATE TABLE fleet_ledger(name VARCHAR(32) PRIMARY KEY, generation INT, rounds INT)",
    )
    .await;
    let values: Vec<String> = ledger
        .iter()
        .map(|(name, generation)| format!("('{name}', {generation}, 1)"))
        .collect();
    run(
        &mut c,
        &format!(
            "INSERT INTO fleet_ledger (name, generation, rounds) VALUES {}",
            values.join(", ")
        ),
    )
    .await;
    assert_eq!(
        ints(&mut c, "SELECT COUNT(*) FROM fleet_ledger").await,
        vec![ROUNDS as i64],
        "one ledger row per group"
    );
    assert_eq!(
        ints(&mut c, "SELECT SUM(generation) FROM fleet_ledger").await,
        vec![ROUNDS as i64],
        "every churned group reached generation 1"
    );
    assert_eq!(
        ints(
            &mut c,
            "SELECT COUNT(*) FROM fleet_ledger WHERE name LIKE 'churn%'"
        )
        .await,
        vec![ROUNDS as i64],
        "every churned group is named churn<N>"
    );
    scenario_common::perf::scenario("fleet114", t0);
}
