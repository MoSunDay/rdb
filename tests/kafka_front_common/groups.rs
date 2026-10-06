//! Client-side helpers for the consumer-group wire APIs (P3 e2e):
//! hand-encode requests the way a Kafka client would, drive them
//! through the REAL binary over TCP, and decode the replies. Only the
//! classic versions needed by the scenarios are modeled (JoinGroup v1,
//! SyncGroup v1, Heartbeat v0, LeaveGroup v0, DescribeGroups v0,
//! FindCoordinator v1, OffsetCommit v2 / OffsetFetch v0 reused from
//! the P2 surface for the fencing assertions).

// Each e2e binary mounts only the helpers its scenarios drive.
#![allow(dead_code)]

use rdb::kafka::frame::Reader;
use rdb::kafka::frame::{
    put_array_len, put_bytes, put_i32, put_i64, put_nullable_string, put_string,
};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

use super::{kafka_req, kafka_round};

/// One leader-only member row of a JoinGroup reply.
pub struct MemberRow {
    pub member_id: String,
    pub metadata: Vec<u8>,
}

/// The decoded fields of a JoinGroup v1 reply.
pub struct JoinView {
    pub error: i16,
    pub generation: i32,
    pub protocol_name: Option<String>,
    pub leader: String,
    pub member_id: String,
    pub members: Vec<MemberRow>,
}

/// JoinGroup v1 round: group, session, rebalance, member, protocols.
pub async fn join_v1(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    session_ms: i32,
    rebalance_ms: i32,
    member: &str,
    subscription: &[u8],
) -> JoinView {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, session_ms);
    put_i32(&mut b, rebalance_ms);
    put_string(&mut b, member);
    put_string(&mut b, "consumer");
    put_array_len(&mut b, 1);
    put_string(&mut b, "range");
    put_bytes(&mut b, subscription);
    let payload = kafka_round(sock, &kafka_req(11, 1, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr), "correlation id");
    let body = &payload[r.pos()..];
    let mut r = Reader::new(body);
    let view = JoinView {
        error: r.i16().unwrap(),
        generation: r.i32().unwrap(),
        protocol_name: r.nullable_string().unwrap(),
        leader: r.string().unwrap(),
        member_id: r.string().unwrap(),
        members: (0..r.array_len().unwrap().unwrap_or(0))
            .map(|_| MemberRow {
                member_id: r.string().unwrap(),
                metadata: r.bytes().unwrap().unwrap_or(&[]).to_vec(),
            })
            .collect(),
    };
    assert_eq!(r.remaining(), 0, "join reply fully consumed");
    view
}

/// SyncGroup v1 round: (error, assignment).
pub async fn sync_v1(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    member: &str,
    generation: i32,
    assignments: &[(&str, &[u8])],
) -> (i16, Vec<u8>) {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, generation);
    put_string(&mut b, member);
    put_array_len(&mut b, assignments.len());
    for (id, a) in assignments {
        put_string(&mut b, id);
        put_bytes(&mut b, a);
    }
    let payload = kafka_round(sock, &kafka_req(14, 1, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.i32(), Some(0), "throttle");
    let error = r.i16().unwrap();
    let assignment = r.bytes().unwrap().unwrap_or(&[]).to_vec();
    assert_eq!(r.remaining(), 0);
    (error, assignment)
}

/// Heartbeat v0 round: the error code.
pub async fn heartbeat_v0(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    generation: i32,
    member: &str,
) -> i16 {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, generation);
    put_string(&mut b, member);
    let payload = kafka_round(sock, &kafka_req(12, 0, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    Reader::new(&payload[r.pos()..]).i16().unwrap()
}

/// LeaveGroup v0 round: the error code.
pub async fn leave_v0(sock: &mut TcpStream, corr: i32, group: &str, member: &str) -> i16 {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_string(&mut b, member);
    let payload = kafka_round(sock, &kafka_req(13, 0, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    Reader::new(&payload[r.pos()..]).i16().unwrap()
}

/// The decoded DescribeGroups v0 row of one group.
pub struct DescribeView {
    pub state: String,
    pub protocol_type: String,
    pub protocol: Option<String>,
    pub member_ids: Vec<String>,
}

/// DescribeGroups v0 round for one group.
pub async fn describe_v0(sock: &mut TcpStream, corr: i32, group: &str) -> DescribeView {
    let mut b = Vec::new();
    put_array_len(&mut b, 1);
    put_string(&mut b, group);
    let payload = kafka_round(sock, &kafka_req(15, 0, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i16(), Some(0), "group-level error");
    assert_eq!(r.string().as_deref(), Some(group));
    let view = DescribeView {
        state: r.string().unwrap(),
        protocol_type: r.string().unwrap(),
        protocol: r.nullable_string().unwrap(),
        member_ids: (0..r.array_len().unwrap().unwrap_or(0))
            .map(|_| {
                let id = r.string().unwrap();
                r.string(); // client_id
                r.string(); // client_host
                r.bytes(); // metadata
                r.bytes(); // assignment
                id
            })
            .collect(),
    };
    assert_eq!(r.remaining(), 0, "describe reply fully consumed");
    view
}

/// OffsetCommit v2 round: the first partition's error code.
#[allow(clippy::too_many_arguments)] // one arg per OffsetCommit v2 wire field
pub async fn commit_v2(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    generation: i32,
    member: &str,
    topic: &str,
    partition: i32,
    offset: i64,
) -> i16 {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_i32(&mut b, generation);
    put_string(&mut b, member);
    put_i64(&mut b, -1); // retention
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, partition);
    put_i64(&mut b, offset);
    put_nullable_string(&mut b, None);
    let payload = kafka_round(sock, &kafka_req(8, 2, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    r.i32();
    r.i16().unwrap()
}

/// OffsetFetch v0 round: the partition's committed offset (-1 if none).
pub async fn fetch_offset_v0(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    topic: &str,
    partition: i32,
) -> i64 {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, partition);
    let payload = kafka_round(sock, &kafka_req(9, 0, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    assert_eq!(r.i32(), Some(partition));
    let off = r.i64().unwrap();
    assert_eq!(r.nullable_string(), Some(None), "metadata null");
    assert_eq!(r.i16(), Some(0), "partition error");
    off
}

// ---- Batch 2 admin + SASL helpers (tests/kafka_admin_e2e.rs) ---------

/// OffsetCommit v0 (no generation/member rows) for one partition:
/// creates the pure ledger-only group shape (0x20 rows, no runtime
/// membership, no lite 0x0E group record).
pub async fn commit_v0(
    sock: &mut TcpStream,
    corr: i32,
    group: &str,
    topic: &str,
    offset: i64,
) -> i16 {
    let mut b = Vec::new();
    put_string(&mut b, group);
    put_array_len(&mut b, 1);
    put_string(&mut b, topic);
    put_array_len(&mut b, 1);
    put_i32(&mut b, 0);
    put_i64(&mut b, offset);
    put_string(&mut b, ""); // metadata (unused)
    let payload = kafka_round(sock, &kafka_req(8, 0, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    assert_eq!(r.array_len(), Some(Some(1)));
    r.string();
    assert_eq!(r.array_len(), Some(Some(1)));
    r.i32();
    r.i16().unwrap()
}

/// ListGroups v0/v1: [(id, protocol_type, state)] -- state is only on
/// the wire for v1+; `states` is the v1 states filter (KIP-518).
pub async fn list_groups(
    sock: &mut TcpStream,
    corr: i32,
    version: i16,
    states: &[&str],
) -> Vec<(String, String, String)> {
    let mut b = Vec::new();
    if version >= 1 {
        put_array_len(&mut b, states.len());
        for s in states {
            put_string(&mut b, s);
        }
    }
    let payload = kafka_round(sock, &kafka_req(16, version, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    if version >= 1 {
        assert_eq!(r.i32(), Some(0), "throttle leads on v1");
    }
    assert_eq!(r.i16(), Some(0), "error NONE");
    let n = r.array_len().unwrap().unwrap_or(0);
    (0..n)
        .map(|_| {
            let id = r.string().unwrap();
            let ty = r.string().unwrap();
            let state = if version >= 1 {
                r.string().unwrap()
            } else {
                String::new()
            };
            (id, ty, state)
        })
        .collect()
}

/// DeleteGroups v0/v1: per-group error codes in request order.
pub async fn delete_groups(
    sock: &mut TcpStream,
    corr: i32,
    version: i16,
    groups: &[&str],
) -> Vec<(String, i16)> {
    let mut b = Vec::new();
    put_array_len(&mut b, groups.len());
    for g in groups {
        put_string(&mut b, g);
    }
    let payload = kafka_round(sock, &kafka_req(42, version, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    if version >= 1 {
        assert_eq!(r.i32(), Some(0), "throttle leads on v1");
    }
    let n = r.array_len().unwrap().unwrap_or(0);
    (0..n)
        .map(|_| (r.string().unwrap(), r.i16().unwrap()))
        .collect()
}

/// SaslHandshake v1: the error code (mechanisms always [PLAIN]).
pub async fn sasl_handshake(sock: &mut TcpStream, corr: i32, mechanism: &str) -> i16 {
    let mut b = Vec::new();
    put_string(&mut b, mechanism);
    let payload = kafka_round(sock, &kafka_req(17, 1, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    let code = r.i16().unwrap();
    let n = r.array_len().unwrap().unwrap_or(0);
    for _ in 0..n {
        assert_eq!(
            r.string().as_deref(),
            Some("PLAIN"),
            "only PLAIN is enabled"
        );
    }
    assert_eq!(r.i32(), Some(0), "throttle tail on v1");
    code
}

/// SaslAuthenticate v1 with `authz\0user\0<password>`: (error, message).
pub async fn sasl_auth(sock: &mut TcpStream, corr: i32, password: &str) -> (i16, Option<String>) {
    let mut b = Vec::new();
    let plain = format!("authz\0user\0{password}");
    put_bytes(&mut b, plain.as_bytes());
    let payload = kafka_round(sock, &kafka_req(36, 1, corr, false, &b)).await;
    let mut r = Reader::new(&payload);
    assert_eq!(r.i32(), Some(corr));
    let mut r = Reader::new(&payload[r.pos()..]);
    let code = r.i16().unwrap();
    let msg = r.nullable_string().unwrap();
    assert_eq!(r.bytes().unwrap(), Some(&b""[..]), "empty auth bytes");
    assert_eq!(r.i64(), Some(0), "session lifetime 0");
    (code, msg)
}

/// Expect the peer to have closed: any read fails/eof within 2s.
pub async fn expect_closed(sock: &mut TcpStream, what: &str) {
    let mut b = [0u8; 16];
    match tokio::time::timeout(std::time::Duration::from_secs(2), sock.read(&mut b)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("{what}: expected close, got {} bytes {b:?}", n),
        Err(_) => panic!("{what}: expected close, timed out still open"),
    }
}
