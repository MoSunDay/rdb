//! Kafka admin + SASL e2e (MQ Batch 2) against the REAL binary:
//! ListGroups answers runtime (JoinGroup) UNION ledger-only
//! (OffsetCommit-only) groups; DeleteGroups folds a group's 0x20 rows
//! through the SAME lite teardown XGROUP DESTROY uses (the wire-side
//! exit for the XTRIM/XDEL ledger guard) and evicts runtime state;
//! unknown groups answer 69 while the rest of the batch proceeds. The
//! SASL matrix runs on a second node with `kafka_token` set: pre-auth
//! ApiVersions, unauthenticated traffic dropped, wrong password ->
//! 58 + close, correct PLAIN token -> full produce/fetch surface.

mod common;
mod kafka_front_common;

use kafka_front_common::groups::{
    commit_v0, delete_groups, describe_v0, expect_closed, fetch_offset_v0, join_v1, list_groups,
    sasl_auth, sasl_handshake, sync_v1,
};
use kafka_front_common::{
    kafka_req, kafka_round, resp_one_shot, spawn_kafka_node, spawn_kafka_node_bind, wait_accepting,
};
use rdb::kafka::errors;
use rdb::kafka::frame::{put_array_len, put_i16, put_i32, put_i64, put_string, Reader};
use rdb::kafka::record::{build_batch, parse_batch, BatchRecord};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const TOKEN: &str = "fake-kafka-token-000";
const SESSION_MS: i32 = 30_000;

/// Produce v2 one record: the partition error code.
async fn produce_one(sock: &mut TcpStream, corr: i32, topic: &str, value: &[u8]) -> i16 {
    let batch = build_batch(
        0,
        1_000,
        &[BatchRecord {
            timestamp_delta: 0,
            key: None,
            value: Some(value),
            headers: vec![],
        }],
    );
    let mut b = Vec::new();
    put_i16(&mut b, 1); // acks
    put_i32(&mut b, 5_000);
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_i32(&mut b, batch.len() as i32);
    b.extend_from_slice(&batch);
    let payload = kafka_round(sock, &kafka_req(0, 2, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    r.i32();
    r.i16().unwrap()
}

/// Fetch v4 one partition: (error, hwm, records blob).
async fn fetch_v4(
    sock: &mut TcpStream,
    corr: i32,
    topic: &str,
    offset: i64,
) -> (i16, i64, Vec<u8>) {
    let mut b = Vec::new();
    put_i32(&mut b, -1); // replica_id
    put_i32(&mut b, 0); // max_wait_ms
    put_i32(&mut b, 1); // min_bytes
    put_i32(&mut b, 1 << 20);
    b.push(0); // isolation_level
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_i64(&mut b, offset);
    put_i32(&mut b, 1 << 20);
    let payload = kafka_round(sock, &kafka_req(1, 4, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.i32(), Some(0), "throttle");
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(0), "partition");
    let error = r.i16().unwrap();
    let hwm = r.i64().unwrap();
    r.i64(); // last_stable_offset
    assert_eq!(r.array_len(), Some(None), "aborted null");
    let records = r.bytes().unwrap().unwrap().to_vec();
    (error, hwm, records)
}

/// Send one framed request without reading a reply.
async fn send_only(sock: &mut TcpStream, req: &[u8]) {
    let mut framed = (req.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(req);
    sock.write_all(&framed).await.expect("write req");
}

/// One XADD per explicit id (deterministic ordinals for the guard).
async fn seed_ids(resp: &str, stream: &[u8], ids: &[&str]) {
    for id in ids {
        resp_one_shot(resp, &[b"XADD", stream, id.as_bytes(), b"f", b"v"]).await;
    }
}

#[tokio::test]
async fn list_groups_unions_runtime_and_ledger_only() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-admin1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    resp_one_shot(&resp, &[b"XADD", b"t/q0", b"*", b"k", b"v"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;

    let mut sock = TcpStream::connect(&kafka).await.expect("connect");
    // Ledger-only group: a bare OffsetCommit never creates runtime
    // membership nor a lite 0x0E group record.
    assert_eq!(
        commit_v0(&mut sock, 1, "g-ledger", "t", 1).await,
        errors::NONE
    );
    // Runtime group: the two-phase join dance (KIP-394) + the leader's
    // SyncGroup lands the group in Stable.
    let r = join_v1(&mut sock, 2, "g-live", SESSION_MS, 60_000, "", b"sub[t0]").await;
    assert_eq!(r.error, errors::MEMBER_ID_REQUIRED);
    let id = r.member_id.clone();
    let r = join_v1(&mut sock, 3, "g-live", SESSION_MS, 60_000, &id, b"sub[t0]").await;
    assert_eq!(r.error, errors::NONE);
    let (err, _) = sync_v1(
        &mut sock,
        4,
        "g-live",
        &id,
        r.generation,
        &[(id.as_str(), b"[t0p0]")],
    )
    .await;
    assert_eq!(err, errors::NONE);

    let groups = list_groups(&mut sock, 5, 0, &[]).await;
    assert_eq!(groups.len(), 2, "runtime UNION ledger-only: {groups:?}");
    let by_id: Vec<(String, String)> = groups.iter().map(|g| (g.0.clone(), g.1.clone())).collect();
    assert!(by_id.contains(&("g-ledger".to_string(), "consumer".to_string())));
    assert!(by_id.contains(&("g-live".to_string(), "consumer".to_string())));

    let v1 = list_groups(&mut sock, 6, 1, &[]).await;
    let ledger = v1.iter().find(|g| g.0 == "g-ledger").unwrap();
    assert_eq!(ledger.2, "Empty", "ledger-only placeholder state");
    let live = v1.iter().find(|g| g.0 == "g-live").unwrap();
    assert_eq!(live.2, "Stable");
    // KIP-518 state filter: Empty keeps only the ledger-only group.
    let only_empty = list_groups(&mut sock, 7, 1, &["Empty"]).await;
    assert_eq!(only_empty.len(), 1);
    assert_eq!(only_empty[0].0, "g-ledger");

    // Empty token = SASL face absent: key 17 rides the unknown-api
    // fallback (ApiVersions-shaped UNSUPPORTED_VERSION, not a hang).
    let payload = kafka_round(&mut sock, &kafka_req(17, 1, 8, false, b"PLAIN")).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(8));
    assert_eq!(r.i16(), Some(35), "SASL not offered without a token");
}

#[tokio::test]
async fn delete_groups_clears_rows_runtime_and_trim_guard() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-admin2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    seed_ids(&resp, b"s/q0", &["1-1", "2-1", "3-1", "4-1", "5-1"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;

    let mut sock = TcpStream::connect(&kafka).await.expect("connect");
    assert_eq!(
        commit_v0(&mut sock, 1, "g-guard", "s", 3).await,
        errors::NONE
    );
    // The guard trips while 0x20 rows pin the stream.
    let blocked = resp_one_shot(&resp, &[b"XTRIM", b"s/q0", b"MINID", b"=", b"3-1"]).await;
    assert!(
        String::from_utf8_lossy(&blocked).contains("committed consumer-group offsets"),
        "guard error before delete: {blocked:?}"
    );
    // A live runtime group on ANOTHER stream goes too.
    resp_one_shot(&resp, &[b"XADD", b"t/q0", b"*", b"k", b"v"]).await;
    let r = join_v1(&mut sock, 2, "g-live", SESSION_MS, 60_000, "", b"sub[t0]").await;
    let r = join_v1(
        &mut sock,
        3,
        "g-live",
        SESSION_MS,
        60_000,
        &r.member_id,
        b"sub[t0]",
    )
    .await;
    assert_eq!(r.error, errors::NONE);

    let results = delete_groups(&mut sock, 4, 0, &["g-guard", "g-live"]).await;
    assert_eq!(
        results,
        vec![
            ("g-guard".to_string(), errors::NONE),
            ("g-live".to_string(), errors::NONE),
        ]
    );

    // Wire-side exit: XTRIM now goes through.
    let trimmed = resp_one_shot(&resp, &[b"XTRIM", b"s/q0", b"MINID", b"=", b"3-1"]).await;
    assert!(trimmed.ends_with(b":2\r\n"), "trim released: {trimmed:?}");
    // Ledger rows and runtime state are gone.
    assert_eq!(fetch_offset_v0(&mut sock, 5, "g-guard", "s", 0).await, -1);
    let listed = list_groups(&mut sock, 6, 1, &[]).await;
    assert!(
        listed.iter().all(|g| g.0 != "g-guard" && g.0 != "g-live"),
        "{listed:?}"
    );
    let described = describe_v0(&mut sock, 7, "g-live").await;
    assert_eq!(described.state, "Dead", "runtime entry evicted");
}

#[tokio::test]
async fn delete_groups_unknown_answers_69_and_allows_rebuild() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-admin3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut node = spawn_kafka_node(&dir);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    resp_one_shot(&resp, &[b"XADD", b"t/q0", b"*", b"k", b"v"]).await;
    wait_accepting(&kafka, &mut node, "kafka").await;

    let mut sock = TcpStream::connect(&kafka).await.expect("connect");
    assert_eq!(
        commit_v0(&mut sock, 1, "g-real", "t", 2).await,
        errors::NONE
    );
    let results = delete_groups(&mut sock, 2, 1, &["g-missing", "g-real"]).await;
    assert_eq!(
        results,
        vec![
            ("g-missing".to_string(), errors::GROUP_ID_NOT_FOUND),
            ("g-real".to_string(), errors::NONE),
        ],
        "unknown answers 69, the rest of the batch still processed"
    );
    // Rebuild after delete: fresh commits land, a rejoin starts at a
    // clean generation (no stale members/generation survive).
    assert_eq!(
        commit_v0(&mut sock, 3, "g-real", "t", 1).await,
        errors::NONE
    );
    let r = join_v1(&mut sock, 4, "g-real", SESSION_MS, 60_000, "", b"sub[t0]").await;
    let r = join_v1(
        &mut sock,
        5,
        "g-real",
        SESSION_MS,
        60_000,
        &r.member_id,
        b"sub[t0]",
    )
    .await;
    assert_eq!(r.error, errors::NONE);
    assert_eq!(r.generation, 1, "generation restarts after teardown");
}

#[tokio::test]
async fn sasl_token_matrix() {
    let dir = std::env::temp_dir().join(format!("rdb-kafka-admin4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let extra = format!("kafka_token: \"{TOKEN}\"\n");
    let mut node = spawn_kafka_node_bind(&dir, "", &extra);
    let (resp, kafka) = (node.resp.clone(), node.kafka.clone());
    wait_accepting(&resp, &mut node, "resp").await;
    wait_accepting(&kafka, &mut node, "kafka").await;

    // Pre-auth ApiVersions answers WITH the SASL pair advertised.
    let mut a = TcpStream::connect(&kafka).await.expect("connect a");
    let payload = kafka_round(&mut a, &kafka_req(18, 0, 1, false, b"")).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(1));
    assert_eq!(r.i16(), Some(0));
    let n = r.array_len().unwrap().unwrap_or(0);
    let mut keys = Vec::new();
    for _ in 0..n {
        keys.push(r.i16().unwrap());
        r.i16();
        r.i16();
    }
    assert_eq!(keys.len(), 17, "15 + the SASL pair");
    assert!(keys.contains(&17) && keys.contains(&36));
    // Unauthenticated Produce is dropped without any reply.
    let mut b = Vec::new();
    put_i16(&mut b, 1);
    put_i32(&mut b, 5_000);
    put_array_len(&mut b, 0);
    send_only(&mut a, &kafka_req(0, 2, 2, false, &b)).await;
    expect_closed(&mut a, "unauthenticated produce").await;

    // Wrong mechanism: 33, mechanisms [PLAIN], then close.
    let mut c = TcpStream::connect(&kafka).await.expect("connect c");
    assert_eq!(
        sasl_handshake(&mut c, 1, "SCRAM-SHA-512").await,
        errors::UNSUPPORTED_SASL_MECHANISM
    );
    expect_closed(&mut c, "unsupported mechanism").await;

    // Wrong password: 58 + fixed message, then close.
    let mut d = TcpStream::connect(&kafka).await.expect("connect d");
    assert_eq!(sasl_handshake(&mut d, 1, "PLAIN").await, errors::NONE);
    let (code, msg) = sasl_auth(&mut d, 2, "wrong-token").await;
    assert_eq!(code, errors::SASL_AUTHENTICATION_FAILED);
    assert!(!msg.unwrap().contains(TOKEN), "no token fragments");
    expect_closed(&mut d, "wrong password").await;

    // Correct token: the full surface works post-auth on one conn.
    let mut e = TcpStream::connect(&kafka).await.expect("connect e");
    assert_eq!(sasl_handshake(&mut e, 1, "PLAIN").await, errors::NONE);
    let (code, msg) = sasl_auth(&mut e, 2, TOKEN).await;
    assert_eq!(code, errors::NONE, "{msg:?}");
    resp_one_shot(&resp, &[b"XADD", b"t/q0", b"*", b"seed", b"1"]).await;
    assert_eq!(
        produce_one(&mut e, 3, "t", b"sasl-value").await,
        errors::NONE
    );
    let (error, hwm, records) = fetch_v4(&mut e, 4, "t", 0).await;
    assert_eq!(error, errors::NONE);
    assert_eq!(hwm, 2, "seed + produced");
    let batch = parse_batch(&records).expect("batch parses");
    assert_eq!(batch.records.len(), 2);
    assert_eq!(batch.records[1].value, Some(b"sasl-value".to_vec()));
    assert_eq!(
        commit_v0(&mut e, 5, "g-after-auth", "t", 2).await,
        errors::NONE,
        "offsets work post-auth"
    );
    let listed = list_groups(&mut e, 6, 1, &[]).await;
    assert!(listed.iter().any(|g| g.0 == "g-after-auth"));
}
