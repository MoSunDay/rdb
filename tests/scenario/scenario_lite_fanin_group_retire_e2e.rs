//! Multi-store scenario #166 -- a notification hub's fan-in consumer: RESP streams `inc166/a`, `inc166/b` and the
//! Kafka topic `inc166k` (stream `inc166k/p0`) feed ONE group `ops`, read in a single XREADGROUP (`*3` sections in
//! argument order; Kafka records return as `(v, body)` entries); the group's Kafka commit ledger pins the topic stream
//! (XTRIM refused) while RESP streams trim; retire: XACK :1, DELCONSUMER = purged count, DESTROY :1 then :0 (sibling
//! group survives), XINFO GROUPS drops the row, `>` answers NOGROUP, DESTROY on the Kafka stream folds the ledger rows
//! (OffsetFetch -1, XTRIM allowed); the consumed fan-in lands in MySQL.
//! Surfaces: RESP lite + Kafka (CreateTopics, Produce v3, ListOffsets v5, Join/Sync v1, OffsetCommit v2,
//! OffsetFetch v0) + MySQL. One REAL node, fronts kafka + mysql (+ RESP always on).
//! Deviation: CreateTopics rejects `inc166/k` with 17 (mapping.rs:39-45), so the topic is `inc166k`; DELCONSUMER replies
//! the purged count, :0 after XACK (group.rs:458-491); XINFO GROUPS is a 13-element flat row without consumers/pending
//! (info.rs:57-105); `CREATE .. $` seeds committed-id with the last id (group.rs:236-254); last-delivered-id is the
//! PERSISTED head (offset.rs:143); a duplicate-PK INSERT overwrites silently, affected 1 (sql/exec/write.rs:160-195).

use crate::common;
use crate::common::mysql::{aff, connect_root, ddl, ints, rows_ordered, s};
use crate::kafka_front_common;
use crate::scenario_common;
use mysql_async::Value as MVal;
use rdb::kafka::record::BatchRecord;
use scenario_common::kafka::kafka_produce_v3;
use scenario_common::{front_world, int_of, ok, r, rb, stream_ids, FrontSpec};
use kafka_front_common::groups::{commit_v2, join_v1, sync_v1};
use kafka_front_common::topics::{create_topics, fetch_offset_v0_raw, list_offsets_v5};
use std::time::Duration;
use tokio::net::TcpStream;

const A: &[u8] = b"inc166/a";
const B: &[u8] = b"inc166/b";
const K: &str = "inc166k";
const KS: &[u8] = b"inc166k/p0";
const GROUP: &str = "ops";
const SIBLING: &str = "audit";
const C1: &str = "c1";
const SESSION_MS: i32 = 45_000;
const REBALANCE_MS: i32 = 60_000;
const INVALID_TOPIC: i16 = 17;
const MEMBER_ID_REQUIRED: i16 = 79;
const A_IDS: [&str; 3] = ["1700000000000-1", "1700000000000-2", "1700000000000-3"];
const B_IDS: [&str; 2] = ["1700000000000-1", "1700000000000-2"];
const A4: &str = "1700000000000-4";
const B3: &str = "1700000000000-3";
const KAFKA_BODIES: [&str; 2] = ["kafka notice one", "kafka notice two"];
const GUARD: &str = "has committed consumer-group offsets; destroy the owning consumer groups (XGROUP DESTROY) first";
const NOGROUP: &str = "NOGROUP No such key 'inc166/a' or consumer group 'ops'";

/// Full reply of one RESP command as text (quiet-gap reader, ~25-45ms).
async fn drain(a: &str, args: &[&[u8]]) -> String {
    common::lite::text(&common::lite::cmd_full_reply_wait(a, common::TOKEN, args, 20, 10_000).await)
}

/// RESP text of one `[id, [f1, v1, ...]]` entry frame, lengths computed here.
fn entry(id: &str, fields: &[(&str, &str)]) -> String {
    let mut out = format!("*2\r\n${}\r\n{id}\r\n*{}\r\n", id.len(), fields.len() * 2);
    for (f, v) in fields {
        out.push_str(&format!("${}\r\n{f}\r\n${}\r\n{v}\r\n", f.len(), v.len()));
    }
    out
}

/// One `[stream, [entries...]]` section of an XREADGROUP reply.
fn section(stream: &[u8], entries: &[String]) -> String {
    format!("*2\r\n${}\r\n{}\r\n*{}\r\n{}", stream.len(), String::from_utf8_lossy(stream), entries.len(), entries.concat())
}

/// Values of one flat RESP row as text: bulk payloads verbatim, `$-1` -> "nil",
/// `:N` -> "N"; `$N`/`*N` headers are consumed (nested arrays flatten).
fn values(full: &str) -> Vec<String> {
    let toks: Vec<&str> = full.split("\r\n").collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        if let Some(n) = toks[i].strip_prefix('$') {
            if n == "-1" {
                out.push("nil".to_string());
            } else {
                i += 1;
                out.push(toks.get(i).copied().unwrap_or("").to_string());
            }
        } else if let Some(n) = toks[i].strip_prefix(':') {
            out.push(n.to_string());
        }
        i += 1;
    }
    out
}

/// The 13-element rows of an XINFO GROUPS reply, in key order.
fn group_rows(full: &str) -> Vec<Vec<String>> {
    full.split("*13\r\n").skip(1).map(values).collect()
}

/// The 12 fields after the name of a 13-element XINFO GROUPS row for a group
/// whose delivered and committed heads are both `id` (the creation state):
/// `last-delivered-id`, `committed-id`, then the unordered tail (ordered :0,
/// inflight :0, owner nil, epoch :0).
fn group_tail(committed: &str) -> [String; 12] {
    [
        "last-delivered-id".to_string(),
        committed.to_string(),
        "committed-id".to_string(),
        committed.to_string(),
        "ordered".to_string(),
        "0".to_string(),
        "inflight".to_string(),
        "0".to_string(),
        "owner".to_string(),
        "nil".to_string(),
        "epoch".to_string(),
        "0".to_string(),
    ]
}

#[tokio::test]
async fn lite_fanin_group_retire() {
    let _serial = scenario_common::serial_guard().await;
    let t0 = scenario_common::perf::now();
    let spec = FrontSpec { mysql: true, kafka: true, ..Default::default() };
    let (front, a) = front_world("fanin166", &spec).await;
    let g = GROUP.as_bytes();
    let c = C1.as_bytes();

    // a) Two RESP streams seeded with explicit ids; `ops` joins at `$` (nothing
    //    new yet), `audit` on `a` joins at 0-0.
    let a_bodies: Vec<String> = (1..=A_IDS.len()).map(|i| format!("a evt {i}")).collect();
    let b_bodies: Vec<String> = (1..=B_IDS.len()).map(|i| format!("b evt {i}")).collect();
    for (stream, ids, bodies) in [(A, &A_IDS[..], &a_bodies), (B, &B_IDS[..], &b_bodies)] {
        for (id, body) in ids.iter().zip(bodies) {
            assert_eq!(rb(&a, &[b"XADD", stream, id.as_bytes(), b"evt", body.as_bytes()]).await, *id, "XADD echoes the explicit id");
        }
        ok(&r(&a, &[b"XGROUP", b"CREATE", stream, g, b"$"]).await, "ops at $");
    }
    ok(&r(&a, &[b"XGROUP", b"CREATE", A, SIBLING.as_bytes(), b"0-0"]).await, "audit at 0-0");
    assert_eq!(drain(&a, &[b"XREADGROUP", b"GROUP", g, c, b"STREAMS", A, B, b">", b">"]).await, "*-1\r\n", "`$` groups have nothing new");

    // b) Kafka: a slashed name is refused (17); the plain topic is created EMPTY,
    //    so the lite group can be created at 0-0 on its p0 stream; the two
    //    produced records are stream entries, log end == XLEN.
    let mut sock = TcpStream::connect(&front.kafka).await.expect("connect kafka front");
    let mut corr = 10;
    assert_eq!(create_topics(&mut sock, corr, "inc166/k", 1, -1, false).await, INVALID_TOPIC, "a topic name may not carry '/'");
    corr += 1;
    assert_eq!(create_topics(&mut sock, corr, K, 1, -1, false).await, 0, "CreateTopics inc166k");
    corr += 1;
    assert_eq!(int_of(&r(&a, &[b"XLEN", KS]).await), 0, "a created topic is an empty stream");
    ok(&r(&a, &[b"XGROUP", b"CREATE", KS, g, b"0-0"]).await, "ops on the kafka stream at 0-0");
    for (i, body) in KAFKA_BODIES.iter().enumerate() {
        let base = kafka_produce_v3(&front.kafka, corr + 10 + i as i32, K, &[BatchRecord {
            timestamp_delta: 0,
            key: None,
            value: Some(body.as_bytes()),
            headers: Vec::new(),
        }]).await;
        assert_eq!(base, i as i32, "base offset {i}");
    }
    assert_eq!(int_of(&r(&a, &[b"XLEN", KS]).await), 2, "XLEN == two produced records");
    assert_eq!(list_offsets_v5(&mut sock, corr, K, 0, -1).await, (0, -1, 2, -1), "log end 2");
    corr += 1;
    assert_eq!(list_offsets_v5(&mut sock, corr, K, 0, -2).await, (0, -1, 0, -1), "log start 0");
    corr += 1;
    let xr = drain(&a, &[b"XRANGE", KS, b"-", b"+"]).await;
    let k_ids = stream_ids(&xr);
    assert_eq!(k_ids.len(), 2, "two stream ids: {xr}");
    let k_entries: Vec<String> = k_ids.iter().zip(KAFKA_BODIES).map(|(id, b)| entry(id, &[("v", b)])).collect();
    assert_eq!(xr, format!("*2\r\n{}", k_entries.concat()), "Kafka records as (v, body) entries");

    // c) Two-phase join/sync on the Kafka side of the SAME group name, then a
    //    commit through the live member: the ledger holds offset 1.
    let first = join_v1(&mut sock, corr, GROUP, SESSION_MS, REBALANCE_MS, "", b"sub").await;
    corr += 1;
    assert_eq!((first.error, first.generation), (MEMBER_ID_REQUIRED, 0), "empty member id -> 79");
    let member = first.member_id.clone();
    assert!(member.starts_with("rdb-ops-"), "assigned member id: {member}");
    let joined = join_v1(&mut sock, corr, GROUP, SESSION_MS, REBALANCE_MS, &member, b"sub").await;
    corr += 1;
    assert_eq!((joined.error, joined.generation, joined.leader.as_str()), (0, 1, member.as_str()), "rejoin: generation 1, leader");
    assert_eq!(sync_v1(&mut sock, corr, GROUP, &member, 1, &[(member.as_str(), b"[p0]")]).await, (0, b"[p0]".to_vec()), "sync clean");
    corr += 1;
    assert_eq!(commit_v2(&mut sock, corr, GROUP, 1, &member, K, 0, 1).await, 0, "commit offset 1");
    corr += 1;
    assert_eq!(fetch_offset_v0_raw(&mut sock, corr, GROUP, K, 0).await, (0, 1), "OffsetFetch echoes 1");
    corr += 1;

    // d) The ledger guard: the committed offset pins inc166k/p0; the RESP
    //    streams trim freely (a: 3 -> 1 keeps the last entry).
    let refused = drain(&a, &[b"XTRIM", KS, b"MAXLEN", b"1"]).await;
    assert!(refused.starts_with("-ERR stream inc166k/p0 ") && refused.contains(GUARD), "guard text: {refused}");
    assert_eq!(int_of(&r(&a, &[b"XLEN", KS]).await), 2, "the refused trim removed nothing");
    assert_eq!(int_of(&r(&a, &[b"XTRIM", A, b"MAXLEN", b"1"]).await), (A_IDS.len() - 1) as i64, "a trims to one entry");

    // e) Hot block -- the fan-in: two fresh RESP events join the two Kafka
    //    records in ONE XREADGROUP over three streams; a second pull is empty.
    let t = scenario_common::perf::now();
    assert_eq!(rb(&a, &[b"XADD", A, A4.as_bytes(), b"evt", b"a evt 4"]).await, A4, "a4");
    assert_eq!(rb(&a, &[b"XADD", B, B3.as_bytes(), b"evt", b"b evt 3"]).await, B3, "b3");
    let fan = drain(&a, &[b"XREADGROUP", b"GROUP", g, c, b"COUNT", b"2", b"STREAMS", A, B, KS, b">", b">", b">"]).await;
    let want = format!(
        "*3\r\n{}{}{}",
        section(A, &[entry(A4, &[("evt", "a evt 4")])]),
        section(B, &[entry(B3, &[("evt", "b evt 3")])]),
        section(KS, &k_entries)
    );
    assert_eq!(fan, want, "three sections in argument order, COUNT 2 caps the kafka section");
    assert_eq!(drain(&a, &[b"XREADGROUP", b"GROUP", g, c, b"STREAMS", A, B, KS, b">", b">", b">"]).await, "*-1\r\n", "drained");
    scenario_common::perf::within("fanin/read-fanin", t, Duration::from_secs(2));

    // f) The fan-in state: both groups on `a` (one 13-element row each), c1
    //    holds the four entries, then the retire sequence.
    let rows = group_rows(&drain(&a, &[b"XINFO", b"GROUPS", A]).await);
    let audit_row: Vec<String> = std::iter::once(SIBLING.to_string()).chain(group_tail("0-0")).collect();
    let ops_tail: Vec<String> = std::iter::once("ops".to_string()).chain(group_tail(A_IDS[2])).collect();
    assert_eq!(rows.len(), 2, "two groups on a: {rows:?}");
    assert_eq!(rows[0], audit_row, "audit row: last-delivered/committed 0-0");
    assert_eq!((rows[1][0].as_str(), rows[1][4].as_str(), rows[1][5..].to_vec()), (ops_tail[0].as_str(), ops_tail[4].as_str(), ops_tail[5..].to_vec()), "ops row: committed = the $ start, unordered tail: {rows:?}");
    let consumers = values(&drain(&a, &[b"XINFO", b"CONSUMERS", A, g]).await);
    assert_eq!(consumers.len(), 6, "one *6 consumer row: {consumers:?}");
    assert_eq!((consumers[0].as_str(), consumers[1].as_str(), consumers[2].as_str(), consumers[3].as_str()), ("name", C1, "pending", "1"), "c1 holds a4");
    assert!(consumers[4] == "idle" && consumers[5].parse::<u64>().is_ok(), "idle ms is numeric: {consumers:?}");
    // The summary flattens the consumer section into the top array (pending.rs:71-81).
    assert_eq!(drain(&a, &[b"XPENDING", A, g]).await, format!("*5\r\n:1\r\n${}\r\n{A4}\r\n${}\r\n{A4}\r\n$2\r\n{C1}\r\n:1\r\n", A4.len(), A4.len()), "PEL summary on a");
    assert_eq!(int_of(&r(&a, &[b"XACK", A, g, A4.as_bytes()]).await), 1, "ack a4");
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DELCONSUMER", A, g, c]).await), 0, "DELCONSUMER replies the PURGED pending count: 0 after the ack");
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DELCONSUMER", B, g, c]).await), 1, "b3 was still pending under c1: purged 1");
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", A, g]).await), 1, "destroy ops on a");
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", A, g]).await), 0, "destroy again: nothing existed");
    let rows = group_rows(&drain(&a, &[b"XINFO", b"GROUPS", A]).await);
    assert_eq!(rows.iter().map(|r| r[0].as_str()).collect::<Vec<_>>(), [SIBLING], "the sibling group survives");
    assert_eq!(drain(&a, &[b"XREADGROUP", b"GROUP", g, c, b"STREAMS", A, b">"]).await, format!("-{NOGROUP}\r\n"), "a `>` read on a retired group");
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", A, SIBLING.as_bytes()]).await), 1, "retire audit too");
    assert_eq!(drain(&a, &[b"XINFO", b"GROUPS", A]).await, "*0\r\n", "no groups left on a");
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", B, g]).await), 1, "destroy ops on b (pending b3 folded)");

    // g) DESTROY on the Kafka-backed stream folds the ledger rows: the guard
    //    lifts and the committed offset is gone.
    assert_eq!(int_of(&r(&a, &[b"XGROUP", b"DESTROY", KS, g]).await), 1, "destroy ops on the kafka stream");
    assert_eq!(int_of(&r(&a, &[b"XTRIM", KS, b"MAXLEN", b"1"]).await), 1, "the trim is allowed again: one victim");
    // The Kafka log end IS the stream's live length: the trim moved it 2 -> 1 (append.rs:369,
    // mapping.rs:68-70) -- the ordinal shift the ledger guard exists to prevent under readers.
    assert_eq!(list_offsets_v5(&mut sock, corr, K, 0, -1).await, (0, -1, 1, -1), "the trim shrank the kafka log end with the stream");
    corr += 1;
    assert_eq!(fetch_offset_v0_raw(&mut sock, corr, GROUP, K, 0).await, (0, -1), "OffsetFetch: no committed offset left");

    // h) MySQL keeps the fan-in tally: four (stream, id) pairs, counted per stream.
    let mut c = connect_root(&front.node).await;
    ddl(&mut c, "CREATE TABLE notify_log (stream VARCHAR(64) NOT NULL, id VARCHAR(32) NOT NULL, PRIMARY KEY (stream, id))").await;
    let pairs: Vec<String> = [(A, A4), (B, B3), (KS, &k_ids[0]), (KS, &k_ids[1])]
        .iter()
        .map(|(st, id)| format!("('{}', '{id}')", String::from_utf8_lossy(st)))
        .collect();
    assert_eq!(aff(&mut c, &format!("INSERT INTO notify_log (stream, id) VALUES {}", pairs.join(", "))).await, 4, "four fan-in rows");
    let mut want: Vec<Vec<MVal>> = [("inc166/a", 1), ("inc166/b", 1), ("inc166k/p0", 2)].iter().map(|(st, n)| vec![s(st), common::mysql::int(*n)]).collect();
    want.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
    assert_eq!(rows_ordered(&mut c, "SELECT stream, COUNT(*) FROM notify_log GROUP BY stream").await, want, "per-stream tally");
    assert_eq!(aff(&mut c, &format!("INSERT INTO notify_log (stream, id) VALUES ('inc166/a', '{A4}')")).await, 1, "a duplicate PK is a silent overwrite, not 1062");
    assert_eq!(ints(&mut c, "SELECT COUNT(*) FROM notify_log").await, vec![4], "still four rows");
    scenario_common::perf::scenario("fanin", t0);
}
